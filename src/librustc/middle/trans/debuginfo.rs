// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*!
# Debug Info Module

This module serves the purpose of generating debug symbols. We use LLVM's
[source level debugging](http://llvm.org/docs/SourceLevelDebugging.html)
features for generating the debug information. The general principle is this:

Given the right metadata in the LLVM IR, the LLVM code generator is able to
create DWARF debug symbols for the given code. The
[metadata](http://llvm.org/docs/LangRef.html#metadata-type) is structured much
like DWARF *debugging information entries* (DIE), representing type information
such as datatype layout, function signatures, block layout, variable location
and scope information, etc. It is the purpose of this module to generate correct
metadata and insert it into the LLVM IR.

As the exact format of metadata trees may change between different LLVM
versions, we now use LLVM
[DIBuilder](http://llvm.org/docs/doxygen/html/classllvm_1_1DIBuilder.html) to
create metadata where possible. This will hopefully ease the adaption of this
module to future LLVM versions.

The public API of the module is a set of functions that will insert the correct
metadata into the LLVM IR when called with the right parameters. The module is
thus driven from an outside client with functions like
`debuginfo::create_local_var_metadata(bcx: block, local: &ast::local)`.

Internally the module will try to reuse already created metadata by utilizing a
cache. The way to get a shared metadata node when needed is thus to just call
the corresponding function in this module:

    let file_metadata = file_metadata(crate_context, path);

The function will take care of probing the cache for an existing node for that
exact file path.

All private state used by the module is stored within either the
CrateDebugContext struct (owned by the CrateContext) or the FunctionDebugContext
(owned by the FunctionContext).

This file consists of three conceptual sections:
1. The public interface of the module
2. Module-internal metadata creation functions
3. Minor utility functions


## Recursive Types

Some kinds of types, such as structs and enums can be recursive. That means that
the type definition of some type X refers to some other type which in turn
(transitively) refers to X. This introduces cycles into the type referral graph.
A naive algorithm doing an on-demand, depth-first traversal of this graph when
describing types, can get trapped in an endless loop when it reaches such a
cycle.

For example, the following simple type for a singly-linked list...

```
struct List {
    value: int,
    tail: Option<Box<List>>,
}
```

will generate the following callstack with a naive DFS algorithm:

```
describe(t = List)
  describe(t = int)
  describe(t = Option<Box<List>>)
    describe(t = Box<List>)
      describe(t = List) // at the beginning again...
      ...
```

To break cycles like these, we use "forward declarations". That is, when the
algorithm encounters a possibly recursive type (any struct or enum), it
immediately creates a type description node and inserts it into the cache
*before* describing the members of the type. This type description is just a
stub (as type members are not described and added to it yet) but it allows the
algorithm to already refer to the type. After the stub is inserted into the
cache, the algorithm continues as before. If it now encounters a recursive
reference, it will hit the cache and does not try to describe the type anew.

This behaviour is encapsulated in the 'RecursiveTypeDescription' enum, which
represents a kind of continuation, storing all state needed to continue
traversal at the type members after the type has been registered with the cache.
(This implementation approach might be a tad over-engineered and may change in
the future)


## Source Locations and Line Information

In addition to data type descriptions the debugging information must also allow
to map machine code locations back to source code locations in order to be useful.
This functionality is also handled in this module. The following functions allow
to control source mappings:

+ set_source_location()
+ clear_source_location()
+ start_emitting_source_locations()

`set_source_location()` allows to set the current source location. All IR
instructions created after a call to this function will be linked to the given
source location, until another location is specified with
`set_source_location()` or the source location is cleared with
`clear_source_location()`. In the later case, subsequent IR instruction will not
be linked to any source location. As you can see, this is a stateful API
(mimicking the one in LLVM), so be careful with source locations set by previous
calls. It's probably best to not rely on any specific state being present at a
given point in code.

One topic that deserves some extra attention is *function prologues*. At the
beginning of a function's machine code there are typically a few instructions
for loading argument values into allocas and checking if there's enough stack
space for the function to execute. This *prologue* is not visible in the source
code and LLVM puts a special PROLOGUE END marker into the line table at the
first non-prologue instruction of the function. In order to find out where the
prologue ends, LLVM looks for the first instruction in the function body that is
linked to a source location. So, when generating prologue instructions we have
to make sure that we don't emit source location information until the 'real'
function body begins. For this reason, source location emission is disabled by
default for any new function being translated and is only activated after a call
to the third function from the list above, `start_emitting_source_locations()`.
This function should be called right before regularly starting to translate the
top-level block of the given function.

There is one exception to the above rule: `llvm.dbg.declare` instruction must be
linked to the source location of the variable being declared. For function
parameters these `llvm.dbg.declare` instructions typically occur in the middle
of the prologue, however, they are ignored by LLVM's prologue detection. The
`create_argument_metadata()` and related functions take care of linking the
`llvm.dbg.declare` instructions to the correct source locations even while
source location emission is still disabled, so there is no need to do anything
special with source location handling here.

## Unique Type Identification

In order for link-time optimization to work properly, LLVM needs a unique type
identifier that tells it across compilation units which types are the same as
others. This type identifier is created by TypeMap::get_unique_type_id_of_type()
using the following algorithm:

(1) Primitive types have their name as ID
(2) Structs, enums and traits have a multipart identifier

    (1) The first part is the SVH (strict version hash) of the crate they were
        originally defined in

    (2) The second part is the ast::NodeId of the definition in their original
        crate

    (3) The final part is a concatenation of the type IDs of their concrete type
        arguments if they are generic types.

(3) Tuple-, pointer and function types are structurally identified, which means
    that they are equivalent if their component types are equivalent (i.e. (int,
    int) is the same regardless in which crate it is used).

This algorithm also provides a stable ID for types that are defined in one crate
but instantiated from metadata within another crate. We just have to take care
to always map crate and node IDs back to the original crate context.

As a side-effect these unique type IDs also help to solve a problem arising from
lifetime parameters. Since lifetime parameters are completely omitted in
debuginfo, more than one `ty::t` instance may map to the same debuginfo type
metadata, that is, some struct `Struct<'a>` may have N instantiations with
different concrete substitutions for `'a`, and thus there will be N `ty::t`
instances for the type `Struct<'a>` even though it is not generic otherwise.
Unfortunately this means that we cannot use `ty::type_id()` as cheap identifier
for type metadata---we have done this in the past, but it led to unnecessary
metadata duplication in the best case and LLVM assertions in the worst. However,
the unique type ID as described above *can* be used as identifier. Since it is
comparatively expensive to construct, though, `ty::type_id()` is still used
additionally as an optimization for cases where the exact same type has been
seen before (which is most of the time). */

use driver::config;
use driver::config::{FullDebugInfo, LimitedDebugInfo, NoDebugInfo};
use llvm;
use llvm::{ModuleRef, ContextRef, ValueRef};
use llvm::debuginfo::*;
use metadata::csearch;
use middle::subst;
use middle::trans::adt;
use middle::trans::common::*;
use middle::trans::machine;
use middle::trans::_match::{BindingInfo, TrByCopy, TrByMove, TrByRef};
use middle::trans::type_of;
use middle::trans::type_::Type;
use middle::trans;
use middle::ty;
use middle::pat_util;
use util::ppaux;

use libc::{c_uint, c_ulonglong, c_longlong};
use std::c_str::{CString, ToCStr};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::HashSet;
use std::gc::Gc;
use std::ptr;
use std::rc::{Rc, Weak};
use syntax::util::interner::Interner;
use syntax::codemap::{Span, Pos};
use syntax::{abi, ast, codemap, ast_util, ast_map};
use syntax::ast_util::PostExpansionMethod;
use syntax::parse::token;
use syntax::parse::token::special_idents;

static DW_LANG_RUST: c_uint = 0x9000;

static DW_TAG_auto_variable: c_uint = 0x100;
static DW_TAG_arg_variable: c_uint = 0x101;

static DW_ATE_boolean: c_uint = 0x02;
static DW_ATE_float: c_uint = 0x04;
static DW_ATE_signed: c_uint = 0x05;
static DW_ATE_unsigned: c_uint = 0x07;
static DW_ATE_unsigned_char: c_uint = 0x08;

static UNKNOWN_LINE_NUMBER: c_uint = 0;
static UNKNOWN_COLUMN_NUMBER: c_uint = 0;

// ptr::null() doesn't work :(
static UNKNOWN_FILE_METADATA: DIFile = (0 as DIFile);
static UNKNOWN_SCOPE_METADATA: DIScope = (0 as DIScope);

static FLAGS_NONE: c_uint = 0;
static FLAGS_ARTIFICAL: c_uint = llvm::debuginfo::FlagArtificial as c_uint;

//=-----------------------------------------------------------------------------
//  Public Interface of debuginfo module
//=-----------------------------------------------------------------------------

#[deriving(Copy, Show, Hash, Eq, PartialEq, Clone)]
struct UniqueTypeId(ast::Name);

// The TypeMap is where the CrateDebugContext holds the type metadata nodes
// created so far. The metadata nodes are indexed by UniqueTypeId, and, for
// faster lookup, also by ty::t. The TypeMap is responsible for creating
// UniqueTypeIds.
struct TypeMap {
    // The UniqueTypeIds created so far
    unique_id_interner: Interner<Rc<String>>,
    // A map from UniqueTypeId to debuginfo metadata for that type. This is a 1:1 mapping.
    unique_id_to_metadata: HashMap<UniqueTypeId, DIType>,
    // A map from ty::type_id() to debuginfo metadata. This is a N:1 mapping.
    type_to_metadata: HashMap<uint, DIType>,
    // A map from ty::type_id() to UniqueTypeId. This is a N:1 mapping.
    type_to_unique_id: HashMap<uint, UniqueTypeId>
}

impl TypeMap {

    fn new() -> TypeMap {
        TypeMap {
            unique_id_interner: Interner::new(),
            type_to_metadata: HashMap::new(),
            unique_id_to_metadata: HashMap::new(),
            type_to_unique_id: HashMap::new(),
        }
    }

    // Adds a ty::t to metadata mapping to the TypeMap. The method will fail if
    // the mapping already exists.
    fn register_type_with_metadata(&mut self,
                                   cx: &CrateContext,
                                   type_: ty::t,
                                   metadata: DIType) {
        if !self.type_to_metadata.insert(ty::type_id(type_), metadata) {
            cx.sess().bug(format!("Type metadata for ty::t '{}' is already in the TypeMap!",
                                   ppaux::ty_to_string(cx.tcx(), type_)).as_slice());
        }
    }

    // Adds a UniqueTypeId to metadata mapping to the TypeMap. The method will
    // fail if the mapping already exists.
    fn register_unique_id_with_metadata(&mut self,
                                        cx: &CrateContext,
                                        unique_type_id: UniqueTypeId,
                                        metadata: DIType) {
        if !self.unique_id_to_metadata.insert(unique_type_id, metadata) {
            let unique_type_id_str = self.get_unique_type_id_as_string(unique_type_id);
            cx.sess().bug(format!("Type metadata for unique id '{}' is already in the TypeMap!",
                                  unique_type_id_str.as_slice()).as_slice());
        }
    }

    fn find_metadata_for_type(&self, type_: ty::t) -> Option<DIType> {
        self.type_to_metadata.find_copy(&ty::type_id(type_))
    }

    fn find_metadata_for_unique_id(&self, unique_type_id: UniqueTypeId) -> Option<DIType> {
        self.unique_id_to_metadata.find_copy(&unique_type_id)
    }

    // Get the string representation of a UniqueTypeId. This method will fail if
    // the id is unknown.
    fn get_unique_type_id_as_string(&self, unique_type_id: UniqueTypeId) -> Rc<String> {
        let UniqueTypeId(interner_key) = unique_type_id;
        self.unique_id_interner.get(interner_key)
    }

    // Get the UniqueTypeId for the given type. If the UniqueTypeId for the given
    // type has been requested before, this is just a table lookup. Otherwise an
    // ID will be generated and stored for later lookup.
    fn get_unique_type_id_of_type(&mut self, cx: &CrateContext, type_: ty::t) -> UniqueTypeId {

        // basic type           -> {:name of the type:}
        // tuple                -> {tuple_(:param-uid:)*}
        // struct               -> {struct_:svh: / :node-id:_<(:param-uid:),*> }
        // enum                 -> {enum_:svh: / :node-id:_<(:param-uid:),*> }
        // enum variant         -> {variant_:variant-name:_:enum-uid:}
        // reference (&)        -> {& :pointee-uid:}
        // mut reference (&mut) -> {&mut :pointee-uid:}
        // ptr (*)              -> {* :pointee-uid:}
        // mut ptr (*mut)       -> {*mut :pointee-uid:}
        // unique ptr (~)       -> {~ :pointee-uid:}
        // @-ptr (@)            -> {@ :pointee-uid:}
        // sized vec ([T, ..x]) -> {[:size:] :element-uid:}
        // unsized vec ([T])    -> {[] :element-uid:}
        // trait (T)            -> {trait_:svh: / :node-id:_<(:param-uid:),*> }
        // closure              -> {<unsafe_> <once_> :store-sigil: |(:param-uid:),* <,_...>| -> \
        //                             :return-type-uid: : (:bounds:)*}
        // function             -> {<unsafe_> <abi_> fn( (:param-uid:)* <,_...> ) -> \
        //                             :return-type-uid:}
        // unique vec box (~[]) -> {HEAP_VEC_BOX<:pointee-uid:>}
        // gc box               -> {GC_BOX<:pointee-uid:>}

        match self.type_to_unique_id.find_copy(&ty::type_id(type_)) {
            Some(unique_type_id) => return unique_type_id,
            None => { /* generate one */}
        };

        let mut unique_type_id = String::with_capacity(256);
        unique_type_id.push_char('{');

        match ty::get(type_).sty {
            ty::ty_nil      |
            ty::ty_bot      |
            ty::ty_bool     |
            ty::ty_char     |
            ty::ty_str      |
            ty::ty_int(_)   |
            ty::ty_uint(_)  |
            ty::ty_float(_) => {
                push_debuginfo_type_name(cx, type_, false, &mut unique_type_id);
            },
            ty::ty_enum(def_id, ref substs) => {
                unique_type_id.push_str("enum ");
                from_def_id_and_substs(self, cx, def_id, substs, &mut unique_type_id);
            },
            ty::ty_struct(def_id, ref substs) => {
                unique_type_id.push_str("struct ");
                from_def_id_and_substs(self, cx, def_id, substs, &mut unique_type_id);
            },
            ty::ty_tup(ref component_types) => {
                unique_type_id.push_str("tuple ");
                for &component_type in component_types.iter() {
                    let component_type_id =
                        self.get_unique_type_id_of_type(cx, component_type);
                    let component_type_id =
                        self.get_unique_type_id_as_string(component_type_id);
                    unique_type_id.push_str(component_type_id.as_slice());
                }
            },
            ty::ty_box(inner_type) => {
                unique_type_id.push_char('@');
                let inner_type_id = self.get_unique_type_id_of_type(cx, inner_type);
                let inner_type_id = self.get_unique_type_id_as_string(inner_type_id);
                unique_type_id.push_str(inner_type_id.as_slice());
            },
            ty::ty_uniq(inner_type) => {
                unique_type_id.push_char('~');
                let inner_type_id = self.get_unique_type_id_of_type(cx, inner_type);
                let inner_type_id = self.get_unique_type_id_as_string(inner_type_id);
                unique_type_id.push_str(inner_type_id.as_slice());
            },
            ty::ty_ptr(ty::mt { ty: inner_type, mutbl } ) => {
                unique_type_id.push_char('*');
                if mutbl == ast::MutMutable {
                    unique_type_id.push_str("mut");
                }

                let inner_type_id = self.get_unique_type_id_of_type(cx, inner_type);
                let inner_type_id = self.get_unique_type_id_as_string(inner_type_id);
                unique_type_id.push_str(inner_type_id.as_slice());
            },
            ty::ty_rptr(_, ty::mt { ty: inner_type, mutbl }) => {
                unique_type_id.push_char('&');
                if mutbl == ast::MutMutable {
                    unique_type_id.push_str("mut");
                }

                let inner_type_id = self.get_unique_type_id_of_type(cx, inner_type);
                let inner_type_id = self.get_unique_type_id_as_string(inner_type_id);
                unique_type_id.push_str(inner_type_id.as_slice());
            },
            ty::ty_vec(inner_type, optional_length) => {
                match optional_length {
                    Some(len) => {
                        unique_type_id.push_str(format!("[{}]", len).as_slice());
                    }
                    None => {
                        unique_type_id.push_str("[]");
                    }
                };

                let inner_type_id = self.get_unique_type_id_of_type(cx, inner_type);
                let inner_type_id = self.get_unique_type_id_as_string(inner_type_id);
                unique_type_id.push_str(inner_type_id.as_slice());
            },
            ty::ty_trait(ref trait_data) => {
                unique_type_id.push_str("trait ");

                from_def_id_and_substs(self,
                                       cx,
                                       trait_data.def_id,
                                       &trait_data.substs,
                                       &mut unique_type_id);
            },
            ty::ty_bare_fn(ty::BareFnTy{ fn_style, abi, ref sig } ) => {
                if fn_style == ast::UnsafeFn {
                    unique_type_id.push_str("unsafe ");
                }

                unique_type_id.push_str(abi.name());

                unique_type_id.push_str(" fn(");

                for &parameter_type in sig.inputs.iter() {
                    let parameter_type_id =
                        self.get_unique_type_id_of_type(cx, parameter_type);
                    let parameter_type_id =
                        self.get_unique_type_id_as_string(parameter_type_id);
                    unique_type_id.push_str(parameter_type_id.as_slice());
                    unique_type_id.push_char(',');
                }

                if sig.variadic {
                    unique_type_id.push_str("...");
                }

                unique_type_id.push_str(")->");
                let return_type_id = self.get_unique_type_id_of_type(cx, sig.output);
                let return_type_id = self.get_unique_type_id_as_string(return_type_id);
                unique_type_id.push_str(return_type_id.as_slice());
            },
            ty::ty_closure(box ty::ClosureTy { fn_style,
                                               onceness,
                                               store,
                                               ref bounds,
                                               ref sig,
                                               abi: _ }) => {
                if fn_style == ast::UnsafeFn {
                    unique_type_id.push_str("unsafe ");
                }

                if onceness == ast::Once {
                    unique_type_id.push_str("once ");
                }

                match store {
                    ty::UniqTraitStore => unique_type_id.push_str("~|"),
                    ty::RegionTraitStore(_, ast::MutMutable) => {
                        unique_type_id.push_str("&mut|")
                    }
                    ty::RegionTraitStore(_, ast::MutImmutable) => {
                        unique_type_id.push_str("&|")
                    }
                };

                for &parameter_type in sig.inputs.iter() {
                    let parameter_type_id =
                        self.get_unique_type_id_of_type(cx, parameter_type);
                    let parameter_type_id =
                        self.get_unique_type_id_as_string(parameter_type_id);
                    unique_type_id.push_str(parameter_type_id.as_slice());
                    unique_type_id.push_char(',');
                }

                if sig.variadic {
                    unique_type_id.push_str("...");
                }

                unique_type_id.push_str("|->");

                let return_type_id = self.get_unique_type_id_of_type(cx, sig.output);
                let return_type_id = self.get_unique_type_id_as_string(return_type_id);
                unique_type_id.push_str(return_type_id.as_slice());

                unique_type_id.push_char(':');

                for bound in bounds.builtin_bounds.iter() {
                    match bound {
                        ty::BoundSend => unique_type_id.push_str("Send"),
                        ty::BoundSized => unique_type_id.push_str("Sized"),
                        ty::BoundCopy => unique_type_id.push_str("Copy"),
                        ty::BoundSync => unique_type_id.push_str("Sync"),
                    };
                    unique_type_id.push_char('+');
                }
            },
            _ => {
                cx.sess().bug(format!("get_unique_type_id_of_type() - unexpected type: {}, {:?}",
                                      ppaux::ty_to_string(cx.tcx(), type_).as_slice(),
                                      ty::get(type_).sty).as_slice())
            }
        };

        unique_type_id.push_char('}');

        // Trim to size before storing permanently
        unique_type_id.shrink_to_fit();

        let key = self.unique_id_interner.intern(Rc::new(unique_type_id));
        self.type_to_unique_id.insert(ty::type_id(type_), UniqueTypeId(key));

        return UniqueTypeId(key);

        fn from_def_id_and_substs(type_map: &mut TypeMap,
                                  cx: &CrateContext,
                                  def_id: ast::DefId,
                                  substs: &subst::Substs,
                                  output: &mut String) {
            use std::num::ToStrRadix;

            // First, find out the 'real' def_id of the type. Items inlined from
            // other crates have to be mapped back to their source.
            let source_def_id = if def_id.krate == ast::LOCAL_CRATE {
                match cx.external_srcs.borrow().find_copy(&def_id.node) {
                    Some(source_def_id) => {
                        // The given def_id identifies the inlined copy of a
                        // type definition, let's take the source of the copy.
                        source_def_id
                    }
                    None => def_id
                }
            } else {
                def_id
            };

            // Get the crate hash as first part of the identifier.
            let crate_hash = if source_def_id.krate == ast::LOCAL_CRATE {
                cx.link_meta.crate_hash.clone()
            } else {
                cx.sess().cstore.get_crate_hash(source_def_id.krate)
            };

            output.push_str(crate_hash.as_str());
            output.push_str("/");
            output.push_str(def_id.node.to_str_radix(16).as_slice());

            // Maybe check that there is no self type here.

            let tps = substs.types.get_slice(subst::TypeSpace);
            if tps.len() > 0 {
                output.push_char('<');

                for &type_parameter in tps.iter() {
                    let param_type_id =
                        type_map.get_unique_type_id_of_type(cx, type_parameter);
                    let param_type_id =
                        type_map.get_unique_type_id_as_string(param_type_id);
                    output.push_str(param_type_id.as_slice());
                    output.push_char(',');
                }

                output.push_char('>');
            }
        }
    }

    // Get the UniqueTypeId for an enum variant. Enum variants are not really
    // types of their own, so they need special handling. We still need a
    // UniqueTypeId for them, since to debuginfo they *are* real types.
    fn get_unique_type_id_of_enum_variant(&mut self,
                                          cx: &CrateContext,
                                          enum_type: ty::t,
                                          variant_name: &str)
                                       -> UniqueTypeId {
        let enum_type_id = self.get_unique_type_id_of_type(cx, enum_type);
        let enum_variant_type_id = format!("{}::{}",
                                           self.get_unique_type_id_as_string(enum_type_id)
                                               .as_slice(),
                                           variant_name);
        let interner_key = self.unique_id_interner.intern(Rc::new(enum_variant_type_id));
        UniqueTypeId(interner_key)
    }

    fn get_unique_type_id_of_gc_box(&mut self,
                                    cx: &CrateContext,
                                    element_type: ty::t)
                                 -> UniqueTypeId {
        let element_type_id = self.get_unique_type_id_of_type(cx, element_type);
        let gc_box_type_id = format!("{{GC_BOX<{}>}}",
                                     self.get_unique_type_id_as_string(element_type_id)
                                         .as_slice());
        let interner_key = self.unique_id_interner.intern(Rc::new(gc_box_type_id));
        UniqueTypeId(interner_key)
    }
}

// Returns from the enclosing function if the type metadata with the given
// unique id can be found in the type map
macro_rules! return_if_metadata_created_in_meantime(
    ($cx: expr, $unique_type_id: expr) => (
        match debug_context($cx).type_map
                                .borrow()
                                .find_metadata_for_unique_id($unique_type_id) {
            Some(metadata) => return MetadataCreationResult::new(metadata, true),
            None => { /* proceed normally */ }
        };
    )
)


/// A context object for maintaining all state needed by the debuginfo module.
pub struct CrateDebugContext {
    llcontext: ContextRef,
    builder: DIBuilderRef,
    current_debug_location: Cell<DebugLocation>,
    created_files: RefCell<HashMap<String, DIFile>>,
    created_enum_disr_types: RefCell<HashMap<ast::DefId, DIType>>,

    type_map: RefCell<TypeMap>,
    namespace_map: RefCell<HashMap<Vec<ast::Name>, Rc<NamespaceTreeNode>>>,

    // This collection is used to assert that composite types (structs, enums,
    // ...) have their members only set once:
    composite_types_completed: RefCell<HashSet<DIType>>,
}

impl CrateDebugContext {
    pub fn new(llmod: ModuleRef) -> CrateDebugContext {
        debug!("CrateDebugContext::new");
        let builder = unsafe { llvm::LLVMDIBuilderCreate(llmod) };
        // DIBuilder inherits context from the module, so we'd better use the same one
        let llcontext = unsafe { llvm::LLVMGetModuleContext(llmod) };
        return CrateDebugContext {
            llcontext: llcontext,
            builder: builder,
            current_debug_location: Cell::new(UnknownLocation),
            created_files: RefCell::new(HashMap::new()),
            created_enum_disr_types: RefCell::new(HashMap::new()),
            type_map: RefCell::new(TypeMap::new()),
            namespace_map: RefCell::new(HashMap::new()),
            composite_types_completed: RefCell::new(HashSet::new()),
        };
    }
}

pub struct FunctionDebugContext {
    repr: FunctionDebugContextRepr,
}

enum FunctionDebugContextRepr {
    FunctionDebugContext(Box<FunctionDebugContextData>),
    DebugInfoDisabled,
    FunctionWithoutDebugInfo,
}

impl FunctionDebugContext {
    fn get_ref<'a>(&'a self,
                   cx: &CrateContext,
                   span: Span)
                   -> &'a FunctionDebugContextData {
        match self.repr {
            FunctionDebugContext(box ref data) => data,
            DebugInfoDisabled => {
                cx.sess().span_bug(span,
                                   FunctionDebugContext::debuginfo_disabled_message());
            }
            FunctionWithoutDebugInfo => {
                cx.sess().span_bug(span,
                                   FunctionDebugContext::should_be_ignored_message());
            }
        }
    }

    fn debuginfo_disabled_message() -> &'static str {
        "debuginfo: Error trying to access FunctionDebugContext although debug info is disabled!"
    }

    fn should_be_ignored_message() -> &'static str {
        "debuginfo: Error trying to access FunctionDebugContext for function that should be \
         ignored by debug info!"
    }
}

struct FunctionDebugContextData {
    scope_map: RefCell<HashMap<ast::NodeId, DIScope>>,
    fn_metadata: DISubprogram,
    argument_counter: Cell<uint>,
    source_locations_enabled: Cell<bool>,
}

enum VariableAccess<'a> {
    // The llptr given is an alloca containing the variable's value
    DirectVariable { alloca: ValueRef },
    // The llptr given is an alloca containing the start of some pointer chain
    // leading to the variable's content.
    IndirectVariable { alloca: ValueRef, address_operations: &'a [ValueRef] }
}

enum VariableKind {
    ArgumentVariable(uint /*index*/),
    LocalVariable,
    CapturedVariable,
}

/// Create any deferred debug metadata nodes
pub fn finalize(cx: &CrateContext) {
    if cx.dbg_cx.is_none() {
        return;
    }

    debug!("finalize");
    compile_unit_metadata(cx);
    unsafe {
        llvm::LLVMDIBuilderFinalize(DIB(cx));
        llvm::LLVMDIBuilderDispose(DIB(cx));
        // Debuginfo generation in LLVM by default uses a higher
        // version of dwarf than OS X currently understands. We can
        // instruct LLVM to emit an older version of dwarf, however,
        // for OS X to understand. For more info see #11352
        // This can be overridden using --llvm-opts -dwarf-version,N.
        if cx.sess().targ_cfg.os == abi::OsMacos ||
            cx.sess().targ_cfg.os == abi::OsiOS {
            "Dwarf Version".with_c_str(
                |s| llvm::LLVMRustAddModuleFlag(cx.llmod, s, 2));
        } else {
            // FIXME(#13611) this is a kludge fix because the linux bots have
            //               gdb 7.4 which doesn't understand dwarf4, we should
            //               do something more graceful here.
            "Dwarf Version".with_c_str(
                |s| llvm::LLVMRustAddModuleFlag(cx.llmod, s, 3));
        }

        // Prevent bitcode readers from deleting the debug info.
        "Debug Info Version".with_c_str(
            |s| llvm::LLVMRustAddModuleFlag(cx.llmod, s,
                                            llvm::LLVMRustDebugMetadataVersion));
    };
}

/// Creates debug information for the given global variable.
///
/// Adds the created metadata nodes directly to the crate's IR.
pub fn create_global_var_metadata(cx: &CrateContext,
                                  node_id: ast::NodeId,
                                  global: ValueRef) {
    if cx.dbg_cx.is_none() {
        return;
    }

    // Don't create debuginfo for globals inlined from other crates. The other
    // crate should already contain debuginfo for it. More importantly, the
    // global might not even exist in un-inlined form anywhere which would lead
    // to a linker errors.
    if cx.external_srcs.borrow().contains_key(&node_id) {
        return;
    }

    let var_item = cx.tcx.map.get(node_id);

    let (ident, span) = match var_item {
        ast_map::NodeItem(item) => {
            match item.node {
                ast::ItemStatic(..) => (item.ident, item.span),
                _ => {
                    cx.sess()
                      .span_bug(item.span,
                                format!("debuginfo::\
                                         create_global_var_metadata() -
                                         Captured var-id refers to \
                                         unexpected ast_item variant: {:?}",
                                        var_item).as_slice())
                }
            }
        },
        _ => cx.sess().bug(format!("debuginfo::create_global_var_metadata() \
                                    - Captured var-id refers to unexpected \
                                    ast_map variant: {:?}",
                                   var_item).as_slice())
    };

    let (file_metadata, line_number) = if span != codemap::DUMMY_SP {
        let loc = span_start(cx, span);
        (file_metadata(cx, loc.file.name.as_slice()), loc.line as c_uint)
    } else {
        (UNKNOWN_FILE_METADATA, UNKNOWN_LINE_NUMBER)
    };

    let is_local_to_unit = is_node_local_to_unit(cx, node_id);
    let variable_type = ty::node_id_to_type(cx.tcx(), node_id);
    let type_metadata = type_metadata(cx, variable_type, span);
    let namespace_node = namespace_for_item(cx, ast_util::local_def(node_id));
    let var_name = token::get_ident(ident).get().to_string();
    let linkage_name =
        namespace_node.mangled_name_of_contained_item(var_name.as_slice());
    let var_scope = namespace_node.scope;

    var_name.as_slice().with_c_str(|var_name| {
        linkage_name.as_slice().with_c_str(|linkage_name| {
            unsafe {
                llvm::LLVMDIBuilderCreateStaticVariable(DIB(cx),
                                                        var_scope,
                                                        var_name,
                                                        linkage_name,
                                                        file_metadata,
                                                        line_number,
                                                        type_metadata,
                                                        is_local_to_unit,
                                                        global,
                                                        ptr::mut_null());
            }
        })
    });
}

/// Creates debug information for the given local variable.
///
/// Adds the created metadata nodes directly to the crate's IR.
pub fn create_local_var_metadata(bcx: &Block, local: &ast::Local) {
    if fn_should_be_ignored(bcx.fcx) {
        return;
    }

    let cx = bcx.ccx();
    let def_map = &cx.tcx.def_map;

    pat_util::pat_bindings(def_map, &*local.pat, |_, node_id, span, path1| {
        let var_ident = path1.node;

        let datum = match bcx.fcx.lllocals.borrow().find_copy(&node_id) {
            Some(datum) => datum,
            None => {
                bcx.sess().span_bug(span,
                    format!("no entry in lllocals table for {:?}",
                            node_id).as_slice());
            }
        };

        let scope_metadata = scope_metadata(bcx.fcx, node_id, span);

        declare_local(bcx,
                      var_ident,
                      datum.ty,
                      scope_metadata,
                      DirectVariable { alloca: datum.val },
                      LocalVariable,
                      span);
    })
}

/// Creates debug information for a variable captured in a closure.
///
/// Adds the created metadata nodes directly to the crate's IR.
pub fn create_captured_var_metadata(bcx: &Block,
                                    node_id: ast::NodeId,
                                    env_data_type: ty::t,
                                    env_pointer: ValueRef,
                                    env_index: uint,
                                    closure_store: ty::TraitStore,
                                    span: Span) {
    if fn_should_be_ignored(bcx.fcx) {
        return;
    }

    let cx = bcx.ccx();

    let ast_item = cx.tcx.map.find(node_id);

    let variable_ident = match ast_item {
        None => {
            cx.sess().span_bug(span, "debuginfo::create_captured_var_metadata: node not found");
        }
        Some(ast_map::NodeLocal(pat)) | Some(ast_map::NodeArg(pat)) => {
            match pat.node {
                ast::PatIdent(_, ref path1, _) => {
                    path1.node
                }
                _ => {
                    cx.sess()
                      .span_bug(span,
                                format!(
                                "debuginfo::create_captured_var_metadata() - \
                                 Captured var-id refers to unexpected \
                                 ast_map variant: {:?}",
                                 ast_item).as_slice());
                }
            }
        }
        _ => {
            cx.sess()
              .span_bug(span,
                        format!("debuginfo::create_captured_var_metadata() - \
                                 Captured var-id refers to unexpected \
                                 ast_map variant: {:?}",
                                ast_item).as_slice());
        }
    };

    let variable_type = node_id_type(bcx, node_id);
    let scope_metadata = bcx.fcx.debug_context.get_ref(cx, span).fn_metadata;

    let llvm_env_data_type = type_of::type_of(cx, env_data_type);
    let byte_offset_of_var_in_env = machine::llelement_offset(cx,
                                                              llvm_env_data_type,
                                                              env_index);

    let address_operations = unsafe {
        [llvm::LLVMDIBuilderCreateOpDeref(Type::i64(cx).to_ref()),
         llvm::LLVMDIBuilderCreateOpPlus(Type::i64(cx).to_ref()),
         C_i64(cx, byte_offset_of_var_in_env as i64),
         llvm::LLVMDIBuilderCreateOpDeref(Type::i64(cx).to_ref())]
    };

    let address_op_count = match closure_store {
        ty::RegionTraitStore(..) => {
            address_operations.len()
        }
        ty::UniqTraitStore => {
            address_operations.len() - 1
        }
    };

    let variable_access = IndirectVariable {
        alloca: env_pointer,
        address_operations: address_operations.slice_to(address_op_count)
    };

    declare_local(bcx,
                  variable_ident,
                  variable_type,
                  scope_metadata,
                  variable_access,
                  CapturedVariable,
                  span);
}

/// Creates debug information for a local variable introduced in the head of a
/// match-statement arm.
///
/// Adds the created metadata nodes directly to the crate's IR.
pub fn create_match_binding_metadata(bcx: &Block,
                                     variable_ident: ast::Ident,
                                     binding: BindingInfo) {
    if fn_should_be_ignored(bcx.fcx) {
        return;
    }

    let scope_metadata = scope_metadata(bcx.fcx, binding.id, binding.span);
    let aops = unsafe {
        [llvm::LLVMDIBuilderCreateOpDeref(bcx.ccx().int_type.to_ref())]
    };
    // Regardless of the actual type (`T`) we're always passed the stack slot (alloca)
    // for the binding. For ByRef bindings that's a `T*` but for ByMove bindings we
    // actually have `T**`. So to get the actual variable we need to dereference once
    // more. For ByCopy we just use the stack slot we created for the binding.
    let var_type = match binding.trmode {
        TrByCopy(llbinding) => DirectVariable {
            alloca: llbinding
        },
        TrByMove => IndirectVariable {
            alloca: binding.llmatch,
            address_operations: aops
        },
        TrByRef => DirectVariable {
            alloca: binding.llmatch
        }
    };

    declare_local(bcx,
                  variable_ident,
                  binding.ty,
                  scope_metadata,
                  var_type,
                  LocalVariable,
                  binding.span);
}

/// Creates debug information for the given function argument.
///
/// Adds the created metadata nodes directly to the crate's IR.
pub fn create_argument_metadata(bcx: &Block, arg: &ast::Arg) {
    if fn_should_be_ignored(bcx.fcx) {
        return;
    }

    let fcx = bcx.fcx;
    let cx = fcx.ccx;

    let def_map = &cx.tcx.def_map;
    let scope_metadata = bcx.fcx.debug_context.get_ref(cx, arg.pat.span).fn_metadata;

    pat_util::pat_bindings(def_map, &*arg.pat, |_, node_id, span, path1| {
        let llarg = match bcx.fcx.llargs.borrow().find_copy(&node_id) {
            Some(v) => v,
            None => {
                bcx.sess().span_bug(span,
                    format!("no entry in llargs table for {:?}",
                            node_id).as_slice());
            }
        };

        if unsafe { llvm::LLVMIsAAllocaInst(llarg.val) } == ptr::mut_null() {
            cx.sess().span_bug(span, "debuginfo::create_argument_metadata() - \
                                    Referenced variable location is not an alloca!");
        }

        let argument_index = {
            let counter = &fcx.debug_context.get_ref(cx, span).argument_counter;
            let argument_index = counter.get();
            counter.set(argument_index + 1);
            argument_index
        };

        declare_local(bcx,
                      path1.node,
                      llarg.ty,
                      scope_metadata,
                      DirectVariable { alloca: llarg.val },
                      ArgumentVariable(argument_index),
                      span);
    })
}

/// Sets the current debug location at the beginning of the span.
///
/// Maps to a call to llvm::LLVMSetCurrentDebugLocation(...). The node_id
/// parameter is used to reliably find the correct visibility scope for the code
/// position.
pub fn set_source_location(fcx: &FunctionContext,
                           node_id: ast::NodeId,
                           span: Span) {
    match fcx.debug_context.repr {
        DebugInfoDisabled => return,
        FunctionWithoutDebugInfo => {
            set_debug_location(fcx.ccx, UnknownLocation);
            return;
        }
        FunctionDebugContext(box ref function_debug_context) => {
            let cx = fcx.ccx;

            debug!("set_source_location: {}", cx.sess().codemap().span_to_string(span));

            if function_debug_context.source_locations_enabled.get() {
                let loc = span_start(cx, span);
                let scope = scope_metadata(fcx, node_id, span);

                set_debug_location(cx, DebugLocation::new(scope,
                                                          loc.line,
                                                          loc.col.to_uint()));
            } else {
                set_debug_location(cx, UnknownLocation);
            }
        }
    }
}

/// Clears the current debug location.
///
/// Instructions generated hereafter won't be assigned a source location.
pub fn clear_source_location(fcx: &FunctionContext) {
    if fn_should_be_ignored(fcx) {
        return;
    }

    set_debug_location(fcx.ccx, UnknownLocation);
}

/// Enables emitting source locations for the given functions.
///
/// Since we don't want source locations to be emitted for the function prelude,
/// they are disabled when beginning to translate a new function. This functions
/// switches source location emitting on and must therefore be called before the
/// first real statement/expression of the function is translated.
pub fn start_emitting_source_locations(fcx: &FunctionContext) {
    match fcx.debug_context.repr {
        FunctionDebugContext(box ref data) => {
            data.source_locations_enabled.set(true)
        },
        _ => { /* safe to ignore */ }
    }
}

/// Creates the function-specific debug context.
///
/// Returns the FunctionDebugContext for the function which holds state needed
/// for debug info creation. The function may also return another variant of the
/// FunctionDebugContext enum which indicates why no debuginfo should be created
/// for the function.
pub fn create_function_debug_context(cx: &CrateContext,
                                     fn_ast_id: ast::NodeId,
                                     param_substs: &param_substs,
                                     llfn: ValueRef) -> FunctionDebugContext {
    if cx.sess().opts.debuginfo == NoDebugInfo {
        return FunctionDebugContext { repr: DebugInfoDisabled };
    }

    // Clear the debug location so we don't assign them in the function prelude.
    // Do this here already, in case we do an early exit from this function.
    set_debug_location(cx, UnknownLocation);

    if fn_ast_id == -1 {
        return FunctionDebugContext { repr: FunctionWithoutDebugInfo };
    }

    let empty_generics = ast_util::empty_generics();

    let fnitem = cx.tcx.map.get(fn_ast_id);

    let (ident, fn_decl, generics, top_level_block, span, has_path) = match fnitem {
        ast_map::NodeItem(ref item) => {
            if contains_nodebug_attribute(item.attrs.as_slice()) {
                return FunctionDebugContext { repr: FunctionWithoutDebugInfo };
            }

            match item.node {
                ast::ItemFn(fn_decl, _, _, ref generics, top_level_block) => {
                    (item.ident, fn_decl, generics, top_level_block, item.span, true)
                }
                _ => {
                    cx.sess().span_bug(item.span,
                        "create_function_debug_context: item bound to non-function");
                }
            }
        }
        ast_map::NodeImplItem(ref item) => {
            match **item {
                ast::MethodImplItem(ref method) => {
                    if contains_nodebug_attribute(method.attrs.as_slice()) {
                        return FunctionDebugContext {
                            repr: FunctionWithoutDebugInfo
                        };
                    }

                    (method.pe_ident(),
                     method.pe_fn_decl(),
                     method.pe_generics(),
                     method.pe_body(),
                     method.span,
                     true)
                }
            }
        }
        ast_map::NodeExpr(ref expr) => {
            match expr.node {
                ast::ExprFnBlock(_, fn_decl, top_level_block) |
                ast::ExprProc(fn_decl, top_level_block) |
                ast::ExprUnboxedFn(_, _, fn_decl, top_level_block) => {
                    let name = format!("fn{}", token::gensym("fn"));
                    let name = token::str_to_ident(name.as_slice());
                    (name, fn_decl,
                        // This is not quite right. It should actually inherit
                        // the generics of the enclosing function.
                        &empty_generics,
                        top_level_block,
                        expr.span,
                        // Don't try to lookup the item path:
                        false)
                }
                _ => cx.sess().span_bug(expr.span,
                        "create_function_debug_context: expected an expr_fn_block here")
            }
        }
        ast_map::NodeTraitItem(ref trait_method) => {
            match **trait_method {
                ast::ProvidedMethod(ref method) => {
                    if contains_nodebug_attribute(method.attrs.as_slice()) {
                        return FunctionDebugContext {
                            repr: FunctionWithoutDebugInfo
                        };
                    }

                    (method.pe_ident(),
                     method.pe_fn_decl(),
                     method.pe_generics(),
                     method.pe_body(),
                     method.span,
                     true)
                }
                _ => {
                    cx.sess()
                      .bug(format!("create_function_debug_context: \
                                    unexpected sort of node: {:?}",
                                    fnitem).as_slice())
                }
            }
        }
        ast_map::NodeForeignItem(..) |
        ast_map::NodeVariant(..) |
        ast_map::NodeStructCtor(..) => {
            return FunctionDebugContext { repr: FunctionWithoutDebugInfo };
        }
        _ => cx.sess().bug(format!("create_function_debug_context: \
                                    unexpected sort of node: {:?}",
                                   fnitem).as_slice())
    };

    // This can be the case for functions inlined from another crate
    if span == codemap::DUMMY_SP {
        return FunctionDebugContext { repr: FunctionWithoutDebugInfo };
    }

    let loc = span_start(cx, span);
    let file_metadata = file_metadata(cx, loc.file.name.as_slice());

    let function_type_metadata = unsafe {
        let fn_signature = get_function_signature(cx,
                                                  fn_ast_id,
                                                  &*fn_decl,
                                                  param_substs,
                                                  span);
        llvm::LLVMDIBuilderCreateSubroutineType(DIB(cx), file_metadata, fn_signature)
    };

    // Get_template_parameters() will append a `<...>` clause to the function
    // name if necessary.
    let mut function_name = String::from_str(token::get_ident(ident).get());
    let template_parameters = get_template_parameters(cx,
                                                      generics,
                                                      param_substs,
                                                      file_metadata,
                                                      &mut function_name);

    // There is no ast_map::Path for ast::ExprFnBlock-type functions. For now,
    // just don't put them into a namespace. In the future this could be improved
    // somehow (storing a path in the ast_map, or construct a path using the
    // enclosing function).
    let (linkage_name, containing_scope) = if has_path {
        let namespace_node = namespace_for_item(cx, ast_util::local_def(fn_ast_id));
        let linkage_name = namespace_node.mangled_name_of_contained_item(
            function_name.as_slice());
        let containing_scope = namespace_node.scope;
        (linkage_name, containing_scope)
    } else {
        (function_name.as_slice().to_string(), file_metadata)
    };

    // Clang sets this parameter to the opening brace of the function's block,
    // so let's do this too.
    let scope_line = span_start(cx, top_level_block.span).line;

    let is_local_to_unit = is_node_local_to_unit(cx, fn_ast_id);

    let fn_metadata = function_name.as_slice().with_c_str(|function_name| {
                          linkage_name.as_slice().with_c_str(|linkage_name| {
            unsafe {
                llvm::LLVMDIBuilderCreateFunction(
                    DIB(cx),
                    containing_scope,
                    function_name,
                    linkage_name,
                    file_metadata,
                    loc.line as c_uint,
                    function_type_metadata,
                    is_local_to_unit,
                    true,
                    scope_line as c_uint,
                    FlagPrototyped as c_uint,
                    cx.sess().opts.optimize != config::No,
                    llfn,
                    template_parameters,
                    ptr::mut_null())
            }
        })
    });

    // Initialize fn debug context (including scope map and namespace map)
    let fn_debug_context = box FunctionDebugContextData {
        scope_map: RefCell::new(HashMap::new()),
        fn_metadata: fn_metadata,
        argument_counter: Cell::new(1),
        source_locations_enabled: Cell::new(false),
    };

    let arg_pats = fn_decl.inputs.iter().map(|arg_ref| arg_ref.pat).collect::<Vec<_>>();
    populate_scope_map(cx,
                       arg_pats.as_slice(),
                       &*top_level_block,
                       fn_metadata,
                       &mut *fn_debug_context.scope_map.borrow_mut());

    return FunctionDebugContext { repr: FunctionDebugContext(fn_debug_context) };

    fn get_function_signature(cx: &CrateContext,
                              fn_ast_id: ast::NodeId,
                              fn_decl: &ast::FnDecl,
                              param_substs: &param_substs,
                              error_span: Span) -> DIArray {
        if cx.sess().opts.debuginfo == LimitedDebugInfo {
            return create_DIArray(DIB(cx), []);
        }

        let mut signature = Vec::with_capacity(fn_decl.inputs.len() + 1);

        // Return type -- llvm::DIBuilder wants this at index 0
        match fn_decl.output.node {
            ast::TyNil => {
                signature.push(ptr::mut_null());
            }
            _ => {
                assert_type_for_node_id(cx, fn_ast_id, error_span);

                let return_type = ty::node_id_to_type(cx.tcx(), fn_ast_id);
                let return_type = return_type.substp(cx.tcx(), param_substs);
                signature.push(type_metadata(cx, return_type, codemap::DUMMY_SP));
            }
        }

        // Arguments types
        for arg in fn_decl.inputs.iter() {
            assert_type_for_node_id(cx, arg.pat.id, arg.pat.span);
            let arg_type = ty::node_id_to_type(cx.tcx(), arg.pat.id);
            let arg_type = arg_type.substp(cx.tcx(), param_substs);
            signature.push(type_metadata(cx, arg_type, codemap::DUMMY_SP));
        }

        return create_DIArray(DIB(cx), signature.as_slice());
    }

    fn get_template_parameters(cx: &CrateContext,
                               generics: &ast::Generics,
                               param_substs: &param_substs,
                               file_metadata: DIFile,
                               name_to_append_suffix_to: &mut String)
                               -> DIArray {
        let self_type = param_substs.substs.self_ty();

        // Only true for static default methods:
        let has_self_type = self_type.is_some();

        if !generics.is_type_parameterized() && !has_self_type {
            return create_DIArray(DIB(cx), []);
        }

        name_to_append_suffix_to.push_char('<');

        // The list to be filled with template parameters:
        let mut template_params: Vec<DIDescriptor> =
            Vec::with_capacity(generics.ty_params.len() + 1);

        // Handle self type
        if has_self_type {
            let actual_self_type = self_type.unwrap();
            // Add self type name to <...> clause of function name
            let actual_self_type_name = compute_debuginfo_type_name(
                cx,
                actual_self_type,
                true);

            name_to_append_suffix_to.push_str(actual_self_type_name.as_slice());

            if generics.is_type_parameterized() {
                name_to_append_suffix_to.push_str(",");
            }

            // Only create type information if full debuginfo is enabled
            if cx.sess().opts.debuginfo == FullDebugInfo {
                let actual_self_type_metadata = type_metadata(cx,
                                                              actual_self_type,
                                                              codemap::DUMMY_SP);

                let ident = special_idents::type_self;

                let param_metadata = token::get_ident(ident).get()
                                                            .with_c_str(|name| {
                    unsafe {
                        llvm::LLVMDIBuilderCreateTemplateTypeParameter(
                            DIB(cx),
                            file_metadata,
                            name,
                            actual_self_type_metadata,
                            ptr::mut_null(),
                            0,
                            0)
                    }
                });

                template_params.push(param_metadata);
            }
        }

        // Handle other generic parameters
        let actual_types = param_substs.substs.types.get_slice(subst::FnSpace);
        for (index, &ast::TyParam{ ident: ident, .. }) in generics.ty_params.iter().enumerate() {
            let actual_type = actual_types[index];
            // Add actual type name to <...> clause of function name
            let actual_type_name = compute_debuginfo_type_name(cx,
                                                               actual_type,
                                                               true);
            name_to_append_suffix_to.push_str(actual_type_name.as_slice());

            if index != generics.ty_params.len() - 1 {
                name_to_append_suffix_to.push_str(",");
            }

            // Again, only create type information if full debuginfo is enabled
            if cx.sess().opts.debuginfo == FullDebugInfo {
                let actual_type_metadata = type_metadata(cx, actual_type, codemap::DUMMY_SP);
                let param_metadata = token::get_ident(ident).get()
                                                            .with_c_str(|name| {
                    unsafe {
                        llvm::LLVMDIBuilderCreateTemplateTypeParameter(
                            DIB(cx),
                            file_metadata,
                            name,
                            actual_type_metadata,
                            ptr::mut_null(),
                            0,
                            0)
                    }
                });
                template_params.push(param_metadata);
            }
        }

        name_to_append_suffix_to.push_char('>');

        return create_DIArray(DIB(cx), template_params.as_slice());
    }
}

//=-----------------------------------------------------------------------------
// Module-Internal debug info creation functions
//=-----------------------------------------------------------------------------

fn is_node_local_to_unit(cx: &CrateContext, node_id: ast::NodeId) -> bool
{
    // The is_local_to_unit flag indicates whether a function is local to the
    // current compilation unit (i.e. if it is *static* in the C-sense). The
    // *reachable* set should provide a good approximation of this, as it
    // contains everything that might leak out of the current crate (by being
    // externally visible or by being inlined into something externally visible).
    // It might better to use the `exported_items` set from `driver::CrateAnalysis`
    // in the future, but (atm) this set is not available in the translation pass.
    !cx.reachable.contains(&node_id)
}

#[allow(non_snake_case)]
fn create_DIArray(builder: DIBuilderRef, arr: &[DIDescriptor]) -> DIArray {
    return unsafe {
        llvm::LLVMDIBuilderGetOrCreateArray(builder, arr.as_ptr(), arr.len() as u32)
    };
}

fn compile_unit_metadata(cx: &CrateContext) {
    let work_dir = &cx.sess().working_dir;
    let compile_unit_name = match cx.sess().local_crate_source_file {
        None => fallback_path(cx),
        Some(ref abs_path) => {
            if abs_path.is_relative() {
                cx.sess().warn("debuginfo: Invalid path to crate's local root source file!");
                fallback_path(cx)
            } else {
                match abs_path.path_relative_from(work_dir) {
                    Some(ref p) if p.is_relative() => {
                            // prepend "./" if necessary
                            let dotdot = b"..";
                            let prefix = &[dotdot[0], ::std::path::SEP_BYTE];
                            let mut path_bytes = Vec::from_slice(p.as_vec());

                            if path_bytes.slice_to(2) != prefix &&
                               path_bytes.slice_to(2) != dotdot {
                                path_bytes.insert(0, prefix[0]);
                                path_bytes.insert(1, prefix[1]);
                            }

                            path_bytes.as_slice().to_c_str()
                        }
                    _ => fallback_path(cx)
                }
            }
        }
    };

    debug!("compile_unit_metadata: {:?}", compile_unit_name);
    let producer = format!("rustc version {}",
                           (option_env!("CFG_VERSION")).expect("CFG_VERSION"));

    let compile_unit_name = compile_unit_name.as_ptr();
    work_dir.as_vec().with_c_str(|work_dir| {
        producer.with_c_str(|producer| {
            "".with_c_str(|flags| {
                "".with_c_str(|split_name| {
                    unsafe {
                        llvm::LLVMDIBuilderCreateCompileUnit(
                            debug_context(cx).builder,
                            DW_LANG_RUST,
                            compile_unit_name,
                            work_dir,
                            producer,
                            cx.sess().opts.optimize != config::No,
                            flags,
                            0,
                            split_name);
                    }
                })
            })
        })
    });

    fn fallback_path(cx: &CrateContext) -> CString {
        cx.link_meta.crate_name.as_slice().to_c_str()
    }
}

fn declare_local(bcx: &Block,
                 variable_ident: ast::Ident,
                 variable_type: ty::t,
                 scope_metadata: DIScope,
                 variable_access: VariableAccess,
                 variable_kind: VariableKind,
                 span: Span) {
    let cx: &CrateContext = bcx.ccx();

    let filename = span_start(cx, span).file.name.clone();
    let file_metadata = file_metadata(cx, filename.as_slice());

    let name = token::get_ident(variable_ident);
    let loc = span_start(cx, span);
    let type_metadata = type_metadata(cx, variable_type, span);

    let (argument_index, dwarf_tag) = match variable_kind {
        ArgumentVariable(index) => (index as c_uint, DW_TAG_arg_variable),
        LocalVariable    |
        CapturedVariable => (0, DW_TAG_auto_variable)
    };

    let (var_alloca, var_metadata) = name.get().with_c_str(|name| {
        match variable_access {
            DirectVariable { alloca } => (
                alloca,
                unsafe {
                    llvm::LLVMDIBuilderCreateLocalVariable(
                        DIB(cx),
                        dwarf_tag,
                        scope_metadata,
                        name,
                        file_metadata,
                        loc.line as c_uint,
                        type_metadata,
                        cx.sess().opts.optimize != config::No,
                        0,
                        argument_index)
                }
            ),
            IndirectVariable { alloca, address_operations } => (
                alloca,
                unsafe {
                    llvm::LLVMDIBuilderCreateComplexVariable(
                        DIB(cx),
                        dwarf_tag,
                        scope_metadata,
                        name,
                        file_metadata,
                        loc.line as c_uint,
                        type_metadata,
                        address_operations.as_ptr(),
                        address_operations.len() as c_uint,
                        argument_index)
                }
            )
        }
    });

    set_debug_location(cx, DebugLocation::new(scope_metadata,
                                              loc.line,
                                              loc.col.to_uint()));
    unsafe {
        let instr = llvm::LLVMDIBuilderInsertDeclareAtEnd(
            DIB(cx),
            var_alloca,
            var_metadata,
            bcx.llbb);

        llvm::LLVMSetInstDebugLocation(trans::build::B(bcx).llbuilder, instr);
    }

    match variable_kind {
        ArgumentVariable(_) | CapturedVariable => {
            assert!(!bcx.fcx
                        .debug_context
                        .get_ref(cx, span)
                        .source_locations_enabled
                        .get());
            set_debug_location(cx, UnknownLocation);
        }
        _ => { /* nothing to do */ }
    }
}

fn file_metadata(cx: &CrateContext, full_path: &str) -> DIFile {
    match debug_context(cx).created_files.borrow().find_equiv(&full_path) {
        Some(file_metadata) => return *file_metadata,
        None => ()
    }

    debug!("file_metadata: {}", full_path);

    // FIXME (#9639): This needs to handle non-utf8 paths
    let work_dir = cx.sess().working_dir.as_str().unwrap();
    let file_name =
        if full_path.starts_with(work_dir) {
            full_path.slice(work_dir.len() + 1u, full_path.len())
        } else {
            full_path
        };

    let file_metadata =
        file_name.with_c_str(|file_name| {
            work_dir.with_c_str(|work_dir| {
                unsafe {
                    llvm::LLVMDIBuilderCreateFile(DIB(cx), file_name, work_dir)
                }
            })
        });

    let mut created_files = debug_context(cx).created_files.borrow_mut();
    created_files.insert(full_path.to_string(), file_metadata);
    return file_metadata;
}

/// Finds the scope metadata node for the given AST node.
fn scope_metadata(fcx: &FunctionContext,
                  node_id: ast::NodeId,
                  span: Span)
               -> DIScope {
    let scope_map = &fcx.debug_context.get_ref(fcx.ccx, span).scope_map;
    match scope_map.borrow().find_copy(&node_id) {
        Some(scope_metadata) => scope_metadata,
        None => {
            let node = fcx.ccx.tcx.map.get(node_id);

            fcx.ccx.sess().span_bug(span,
                format!("debuginfo: Could not find scope info for node {:?}",
                        node).as_slice());
        }
    }
}

fn basic_type_metadata(cx: &CrateContext, t: ty::t) -> DIType {

    debug!("basic_type_metadata: {:?}", ty::get(t));

    let (name, encoding) = match ty::get(t).sty {
        ty::ty_nil => ("()".to_string(), DW_ATE_unsigned),
        ty::ty_bot => ("!".to_string(), DW_ATE_unsigned),
        ty::ty_bool => ("bool".to_string(), DW_ATE_boolean),
        ty::ty_char => ("char".to_string(), DW_ATE_unsigned_char),
        ty::ty_int(int_ty) => match int_ty {
            ast::TyI => ("int".to_string(), DW_ATE_signed),
            ast::TyI8 => ("i8".to_string(), DW_ATE_signed),
            ast::TyI16 => ("i16".to_string(), DW_ATE_signed),
            ast::TyI32 => ("i32".to_string(), DW_ATE_signed),
            ast::TyI64 => ("i64".to_string(), DW_ATE_signed)
        },
        ty::ty_uint(uint_ty) => match uint_ty {
            ast::TyU => ("uint".to_string(), DW_ATE_unsigned),
            ast::TyU8 => ("u8".to_string(), DW_ATE_unsigned),
            ast::TyU16 => ("u16".to_string(), DW_ATE_unsigned),
            ast::TyU32 => ("u32".to_string(), DW_ATE_unsigned),
            ast::TyU64 => ("u64".to_string(), DW_ATE_unsigned)
        },
        ty::ty_float(float_ty) => match float_ty {
            ast::TyF32 => ("f32".to_string(), DW_ATE_float),
            ast::TyF64 => ("f64".to_string(), DW_ATE_float),
        },
        _ => cx.sess().bug("debuginfo::basic_type_metadata - t is invalid type")
    };

    let llvm_type = type_of::type_of(cx, t);
    let (size, align) = size_and_align_of(cx, llvm_type);
    let ty_metadata = name.with_c_str(|name| {
        unsafe {
            llvm::LLVMDIBuilderCreateBasicType(
                DIB(cx),
                name,
                bytes_to_bits(size),
                bytes_to_bits(align),
                encoding)
        }
    });

    return ty_metadata;
}

fn pointer_type_metadata(cx: &CrateContext,
                         pointer_type: ty::t,
                         pointee_type_metadata: DIType)
                      -> DIType {
    let pointer_llvm_type = type_of::type_of(cx, pointer_type);
    let (pointer_size, pointer_align) = size_and_align_of(cx, pointer_llvm_type);
    let name = compute_debuginfo_type_name(cx, pointer_type, false);
    let ptr_metadata = name.as_slice().with_c_str(|name| {
        unsafe {
            llvm::LLVMDIBuilderCreatePointerType(
                DIB(cx),
                pointee_type_metadata,
                bytes_to_bits(pointer_size),
                bytes_to_bits(pointer_align),
                name)
        }
    });
    return ptr_metadata;
}

//=-----------------------------------------------------------------------------
// Common facilities for record-like types (structs, enums, tuples)
//=-----------------------------------------------------------------------------

enum MemberOffset {
    FixedMemberOffset { bytes: uint },
    // For ComputedMemberOffset, the offset is read from the llvm type definition
    ComputedMemberOffset
}

// Description of a type member, which can either be a regular field (as in
// structs or tuples) or an enum variant
struct MemberDescription {
    name: String,
    llvm_type: Type,
    type_metadata: DIType,
    offset: MemberOffset,
    flags: c_uint
}

// A factory for MemberDescriptions. It produces a list of member descriptions
// for some record-like type. MemberDescriptionFactories are used to defer the
// creation of type member descriptions in order to break cycles arising from
// recursive type definitions.
enum MemberDescriptionFactory {
    StructMDF(StructMemberDescriptionFactory),
    TupleMDF(TupleMemberDescriptionFactory),
    EnumMDF(EnumMemberDescriptionFactory),
    VariantMDF(VariantMemberDescriptionFactory)
}

impl MemberDescriptionFactory {
    fn create_member_descriptions(&self, cx: &CrateContext) -> Vec<MemberDescription> {
        match *self {
            StructMDF(ref this) => {
                this.create_member_descriptions(cx)
            }
            TupleMDF(ref this) => {
                this.create_member_descriptions(cx)
            }
            EnumMDF(ref this) => {
                this.create_member_descriptions(cx)
            }
            VariantMDF(ref this) => {
                this.create_member_descriptions(cx)
            }
        }
    }
}

// A description of some recursive type. It can either be already finished (as
// with FinalMetadata) or it is not yet finished, but contains all information
// needed to generate the missing parts of the description. See the documentation
// section on Recursive Types at the top of this file for more information.
enum RecursiveTypeDescription {
    UnfinishedMetadata {
        unfinished_type: ty::t,
        unique_type_id: UniqueTypeId,
        metadata_stub: DICompositeType,
        llvm_type: Type,
        member_description_factory: MemberDescriptionFactory,
    },
    FinalMetadata(DICompositeType)
}

fn create_and_register_recursive_type_forward_declaration(
    cx: &CrateContext,
    unfinished_type: ty::t,
    unique_type_id: UniqueTypeId,
    metadata_stub: DICompositeType,
    llvm_type: Type,
    member_description_factory: MemberDescriptionFactory)
 -> RecursiveTypeDescription {

    // Insert the stub into the TypeMap in order to allow for recursive references
    let mut type_map = debug_context(cx).type_map.borrow_mut();
    type_map.register_unique_id_with_metadata(cx, unique_type_id, metadata_stub);
    type_map.register_type_with_metadata(cx, unfinished_type, metadata_stub);

    UnfinishedMetadata {
        unfinished_type: unfinished_type,
        unique_type_id: unique_type_id,
        metadata_stub: metadata_stub,
        llvm_type: llvm_type,
        member_description_factory: member_description_factory,
    }
}

impl RecursiveTypeDescription {
    // Finishes up the description of the type in question (mostly by providing
    // descriptions of the fields of the given type) and returns the final type metadata.
    fn finalize(&self, cx: &CrateContext) -> MetadataCreationResult {
        match *self {
            FinalMetadata(metadata) => MetadataCreationResult::new(metadata, false),
            UnfinishedMetadata {
                unfinished_type,
                unique_type_id,
                metadata_stub,
                llvm_type,
                ref member_description_factory,
                ..
            } => {
                // Make sure that we have a forward declaration of the type in
                // the TypeMap so that recursive references are possible. This
                // will always be the case if the RecursiveTypeDescription has
                // been properly created through the
                // create_and_register_recursive_type_forward_declaration() function.
                {
                    let type_map = debug_context(cx).type_map.borrow();
                    if type_map.find_metadata_for_unique_id(unique_type_id).is_none() ||
                       type_map.find_metadata_for_type(unfinished_type).is_none() {
                        cx.sess().bug(format!("Forward declaration of potentially recursive type \
                                              '{}' was not found in TypeMap!",
                                              ppaux::ty_to_string(cx.tcx(), unfinished_type))
                                      .as_slice());
                    }
                }

                // ... then create the member descriptions ...
                let member_descriptions =
                    member_description_factory.create_member_descriptions(cx);

                // ... and attach them to the stub to complete it.
                set_members_of_composite_type(cx,
                                              metadata_stub,
                                              llvm_type,
                                              member_descriptions.as_slice());
                return MetadataCreationResult::new(metadata_stub, true);
            }
        }
    }
}


//=-----------------------------------------------------------------------------
// Structs
//=-----------------------------------------------------------------------------

// Creates MemberDescriptions for the fields of a struct
struct StructMemberDescriptionFactory {
    fields: Vec<ty::field>,
    is_simd: bool,
    span: Span,
}

impl StructMemberDescriptionFactory {
    fn create_member_descriptions(&self, cx: &CrateContext) -> Vec<MemberDescription> {
        if self.fields.len() == 0 {
            return Vec::new();
        }

        let field_size = if self.is_simd {
            machine::llsize_of_alloc(cx, type_of::type_of(cx, self.fields.get(0).mt.ty)) as uint
        } else {
            0xdeadbeef
        };

        self.fields.iter().enumerate().map(|(i, field)| {
            let name = if field.ident.name == special_idents::unnamed_field.name {
                "".to_string()
            } else {
                token::get_ident(field.ident).get().to_string()
            };

            let offset = if self.is_simd {
                assert!(field_size != 0xdeadbeef);
                FixedMemberOffset { bytes: i * field_size }
            } else {
                ComputedMemberOffset
            };

            MemberDescription {
                name: name,
                llvm_type: type_of::type_of(cx, field.mt.ty),
                type_metadata: type_metadata(cx, field.mt.ty, self.span),
                offset: offset,
                flags: FLAGS_NONE,
            }
        }).collect()
    }
}


fn prepare_struct_metadata(cx: &CrateContext,
                           struct_type: ty::t,
                           def_id: ast::DefId,
                           substs: &subst::Substs,
                           unique_type_id: UniqueTypeId,
                           span: Span)
                        -> RecursiveTypeDescription {
    let struct_name = compute_debuginfo_type_name(cx, struct_type, false);
    let struct_llvm_type = type_of::type_of(cx, struct_type);

    let (containing_scope, _) = get_namespace_and_span_for_item(cx, def_id);

    let struct_metadata_stub = create_struct_stub(cx,
                                                  struct_llvm_type,
                                                  struct_name.as_slice(),
                                                  unique_type_id,
                                                  containing_scope);

    let fields = ty::struct_fields(cx.tcx(), def_id, substs);

    create_and_register_recursive_type_forward_declaration(
        cx,
        struct_type,
        unique_type_id,
        struct_metadata_stub,
        struct_llvm_type,
        StructMDF(StructMemberDescriptionFactory {
            fields: fields,
            is_simd: ty::type_is_simd(cx.tcx(), struct_type),
            span: span,
        })
    )
}


//=-----------------------------------------------------------------------------
// Tuples
//=-----------------------------------------------------------------------------

// Creates MemberDescriptions for the fields of a tuple
struct TupleMemberDescriptionFactory {
    component_types: Vec<ty::t> ,
    span: Span,
}

impl TupleMemberDescriptionFactory {
    fn create_member_descriptions(&self, cx: &CrateContext)
                                  -> Vec<MemberDescription> {
        self.component_types.iter().map(|&component_type| {
            MemberDescription {
                name: "".to_string(),
                llvm_type: type_of::type_of(cx, component_type),
                type_metadata: type_metadata(cx, component_type, self.span),
                offset: ComputedMemberOffset,
                flags: FLAGS_NONE,
            }
        }).collect()
    }
}

fn prepare_tuple_metadata(cx: &CrateContext,
                          tuple_type: ty::t,
                          component_types: &[ty::t],
                          unique_type_id: UniqueTypeId,
                          span: Span)
                       -> RecursiveTypeDescription {
    let tuple_name = compute_debuginfo_type_name(cx, tuple_type, false);
    let tuple_llvm_type = type_of::type_of(cx, tuple_type);

    create_and_register_recursive_type_forward_declaration(
        cx,
        tuple_type,
        unique_type_id,
        create_struct_stub(cx,
                           tuple_llvm_type,
                           tuple_name.as_slice(),
                           unique_type_id,
                           UNKNOWN_SCOPE_METADATA),
        tuple_llvm_type,
        TupleMDF(TupleMemberDescriptionFactory {
            component_types: Vec::from_slice(component_types),
            span: span,
        })
    )
}


//=-----------------------------------------------------------------------------
// Enums
//=-----------------------------------------------------------------------------

// Describes the members of an enum value: An enum is described as a union of
// structs in DWARF. This MemberDescriptionFactory provides the description for
// the members of this union; so for every variant of the given enum, this factory
// will produce one MemberDescription (all with no name and a fixed offset of
// zero bytes).
struct EnumMemberDescriptionFactory {
    enum_type: ty::t,
    type_rep: Rc<adt::Repr>,
    variants: Rc<Vec<Rc<ty::VariantInfo>>>,
    discriminant_type_metadata: Option<DIType>,
    containing_scope: DIScope,
    file_metadata: DIFile,
    span: Span,
}

impl EnumMemberDescriptionFactory {
    fn create_member_descriptions(&self, cx: &CrateContext) -> Vec<MemberDescription> {
        match *self.type_rep {
            adt::General(_, ref struct_defs, _) => {
                let discriminant_info = RegularDiscriminant(self.discriminant_type_metadata
                    .expect(""));

                struct_defs
                    .iter()
                    .enumerate()
                    .map(|(i, struct_def)| {
                        let (variant_type_metadata,
                             variant_llvm_type,
                             member_desc_factory) =
                            describe_enum_variant(cx,
                                                  self.enum_type,
                                                  struct_def,
                                                  &**self.variants.get(i),
                                                  discriminant_info,
                                                  self.containing_scope,
                                                  self.span);

                        let member_descriptions = member_desc_factory
                            .create_member_descriptions(cx);

                        set_members_of_composite_type(cx,
                                                      variant_type_metadata,
                                                      variant_llvm_type,
                                                      member_descriptions.as_slice());
                        MemberDescription {
                            name: "".to_string(),
                            llvm_type: variant_llvm_type,
                            type_metadata: variant_type_metadata,
                            offset: FixedMemberOffset { bytes: 0 },
                            flags: FLAGS_NONE
                        }
                    }).collect()
            },
            adt::Univariant(ref struct_def, _) => {
                assert!(self.variants.len() <= 1);

                if self.variants.len() == 0 {
                    vec![]
                } else {
                    let (variant_type_metadata,
                         variant_llvm_type,
                         member_description_factory) =
                        describe_enum_variant(cx,
                                              self.enum_type,
                                              struct_def,
                                              &**self.variants.get(0),
                                              NoDiscriminant,
                                              self.containing_scope,
                                              self.span);

                    let member_descriptions =
                        member_description_factory.create_member_descriptions(cx);

                    set_members_of_composite_type(cx,
                                                  variant_type_metadata,
                                                  variant_llvm_type,
                                                  member_descriptions.as_slice());
                    vec![
                        MemberDescription {
                            name: "".to_string(),
                            llvm_type: variant_llvm_type,
                            type_metadata: variant_type_metadata,
                            offset: FixedMemberOffset { bytes: 0 },
                            flags: FLAGS_NONE
                        }
                    ]
                }
            }
            adt::RawNullablePointer { nndiscr: non_null_variant_index, nnty, .. } => {
                // As far as debuginfo is concerned, the pointer this enum
                // represents is still wrapped in a struct. This is to make the
                // DWARF representation of enums uniform.

                // First create a description of the artificial wrapper struct:
                let non_null_variant = self.variants.get(non_null_variant_index as uint);
                let non_null_variant_ident = non_null_variant.name;
                let non_null_variant_name = token::get_ident(non_null_variant_ident);

                // The llvm type and metadata of the pointer
                let non_null_llvm_type = type_of::type_of(cx, nnty);
                let non_null_type_metadata = type_metadata(cx, nnty, self.span);

                // The type of the artificial struct wrapping the pointer
                let artificial_struct_llvm_type = Type::struct_(cx,
                                                                &[non_null_llvm_type],
                                                                false);

                // For the metadata of the wrapper struct, we need to create a
                // MemberDescription of the struct's single field.
                let sole_struct_member_description = MemberDescription {
                    name: match non_null_variant.arg_names {
                        Some(ref names) => token::get_ident(*names.get(0)).get().to_string(),
                        None => "".to_string()
                    },
                    llvm_type: non_null_llvm_type,
                    type_metadata: non_null_type_metadata,
                    offset: FixedMemberOffset { bytes: 0 },
                    flags: FLAGS_NONE
                };

                let unique_type_id = debug_context(cx).type_map
                                                      .borrow_mut()
                                                      .get_unique_type_id_of_enum_variant(
                                                          cx,
                                                          self.enum_type,
                                                          non_null_variant_name.get());

                // Now we can create the metadata of the artificial struct
                let artificial_struct_metadata =
                    composite_type_metadata(cx,
                                            artificial_struct_llvm_type,
                                            non_null_variant_name.get(),
                                            unique_type_id,
                                            &[sole_struct_member_description],
                                            self.containing_scope,
                                            self.file_metadata,
                                            codemap::DUMMY_SP);

                // Encode the information about the null variant in the union
                // member's name.
                let null_variant_index = (1 - non_null_variant_index) as uint;
                let null_variant_ident = self.variants.get(null_variant_index).name;
                let null_variant_name = token::get_ident(null_variant_ident);
                let union_member_name = format!("RUST$ENCODED$ENUM${}${}",
                                                0u,
                                                null_variant_name);

                // Finally create the (singleton) list of descriptions of union
                // members.
                vec![
                    MemberDescription {
                        name: union_member_name,
                        llvm_type: artificial_struct_llvm_type,
                        type_metadata: artificial_struct_metadata,
                        offset: FixedMemberOffset { bytes: 0 },
                        flags: FLAGS_NONE
                    }
                ]
            },
            adt::StructWrappedNullablePointer { nonnull: ref struct_def,
                                                nndiscr,
                                                ptrfield, ..} => {
                // Create a description of the non-null variant
                let (variant_type_metadata, variant_llvm_type, member_description_factory) =
                    describe_enum_variant(cx,
                                          self.enum_type,
                                          struct_def,
                                          &**self.variants.get(nndiscr as uint),
                                          OptimizedDiscriminant(ptrfield),
                                          self.containing_scope,
                                          self.span);

                let variant_member_descriptions =
                    member_description_factory.create_member_descriptions(cx);

                set_members_of_composite_type(cx,
                                              variant_type_metadata,
                                              variant_llvm_type,
                                              variant_member_descriptions.as_slice());

                // Encode the information about the null variant in the union
                // member's name.
                let null_variant_index = (1 - nndiscr) as uint;
                let null_variant_ident = self.variants.get(null_variant_index).name;
                let null_variant_name = token::get_ident(null_variant_ident);
                let discrfield = match ptrfield {
                    adt::ThinPointer(field) => format!("{}", field),
                    adt::FatPointer(field, pair) => format!("{}${}", field, pair)
                };
                let union_member_name = format!("RUST$ENCODED$ENUM${}${}",
                                                discrfield,
                                                null_variant_name);

                // Create the (singleton) list of descriptions of union members.
                vec![
                    MemberDescription {
                        name: union_member_name,
                        llvm_type: variant_llvm_type,
                        type_metadata: variant_type_metadata,
                        offset: FixedMemberOffset { bytes: 0 },
                        flags: FLAGS_NONE
                    }
                ]
            },
            adt::CEnum(..) => cx.sess().span_bug(self.span, "This should be unreachable.")
        }
    }
}

// Creates MemberDescriptions for the fields of a single enum variant.
struct VariantMemberDescriptionFactory {
    args: Vec<(String, ty::t)> ,
    discriminant_type_metadata: Option<DIType>,
    span: Span,
}

impl VariantMemberDescriptionFactory {
    fn create_member_descriptions(&self, cx: &CrateContext) -> Vec<MemberDescription> {
        self.args.iter().enumerate().map(|(i, &(ref name, ty))| {
            MemberDescription {
                name: name.to_string(),
                llvm_type: type_of::type_of(cx, ty),
                type_metadata: match self.discriminant_type_metadata {
                    Some(metadata) if i == 0 => metadata,
                    _ => type_metadata(cx, ty, self.span)
                },
                offset: ComputedMemberOffset,
                flags: if self.discriminant_type_metadata.is_some() &&  i == 0 {
                    FLAGS_ARTIFICAL
                } else {
                    FLAGS_NONE
                }
            }
        }).collect()
    }
}

enum EnumDiscriminantInfo {
    RegularDiscriminant(DIType),
    OptimizedDiscriminant(adt::PointerField),
    NoDiscriminant
}

// Returns a tuple of (1) type_metadata_stub of the variant, (2) the llvm_type
// of the variant, and (3) a MemberDescriptionFactory for producing the
// descriptions of the fields of the variant. This is a rudimentary version of a
// full RecursiveTypeDescription.
fn describe_enum_variant(cx: &CrateContext,
                         enum_type: ty::t,
                         struct_def: &adt::Struct,
                         variant_info: &ty::VariantInfo,
                         discriminant_info: EnumDiscriminantInfo,
                         containing_scope: DIScope,
                         span: Span)
                      -> (DICompositeType, Type, MemberDescriptionFactory) {
    let variant_llvm_type =
        Type::struct_(cx, struct_def.fields
                                    .iter()
                                    .map(|&t| type_of::type_of(cx, t))
                                    .collect::<Vec<_>>()
                                    .as_slice(),
                      struct_def.packed);
    // Could do some consistency checks here: size, align, field count, discr type

    let variant_name = token::get_ident(variant_info.name);
    let variant_name = variant_name.get();
    let unique_type_id = debug_context(cx).type_map
                                          .borrow_mut()
                                          .get_unique_type_id_of_enum_variant(
                                              cx,
                                              enum_type,
                                              variant_name);

    let metadata_stub = create_struct_stub(cx,
                                           variant_llvm_type,
                                           variant_name,
                                           unique_type_id,
                                           containing_scope);

    // Get the argument names from the enum variant info
    let mut arg_names: Vec<_> = match variant_info.arg_names {
        Some(ref names) => {
            names.iter()
                 .map(|ident| {
                     token::get_ident(*ident).get().to_string().into_string()
                 }).collect()
        }
        None => variant_info.args.iter().map(|_| "".to_string()).collect()
    };

    // If this is not a univariant enum, there is also the (unnamed) discriminant field.
    match discriminant_info {
        RegularDiscriminant(_) => arg_names.insert(0, "".to_string()),
        _ => { /* do nothing */ }
    };

    // Build an array of (field name, field type) pairs to be captured in the factory closure.
    let args: Vec<(String, ty::t)> = arg_names.iter()
        .zip(struct_def.fields.iter())
        .map(|(s, &t)| (s.to_string(), t))
        .collect();

    let member_description_factory =
        VariantMDF(VariantMemberDescriptionFactory {
            args: args,
            discriminant_type_metadata: match discriminant_info {
                RegularDiscriminant(discriminant_type_metadata) => {
                    Some(discriminant_type_metadata)
                }
                _ => None
            },
            span: span,
        });

    (metadata_stub, variant_llvm_type, member_description_factory)
}

fn prepare_enum_metadata(cx: &CrateContext,
                         enum_type: ty::t,
                         enum_def_id: ast::DefId,
                         unique_type_id: UniqueTypeId,
                         span: Span)
                      -> RecursiveTypeDescription {
    let enum_name = compute_debuginfo_type_name(cx, enum_type, false);

    let (containing_scope, definition_span) = get_namespace_and_span_for_item(cx, enum_def_id);
    let loc = span_start(cx, definition_span);
    let file_metadata = file_metadata(cx, loc.file.name.as_slice());

    let variants = ty::enum_variants(cx.tcx(), enum_def_id);

    let enumerators_metadata: Vec<DIDescriptor> = variants
        .iter()
        .map(|v| {
            token::get_ident(v.name).get().with_c_str(|name| {
                unsafe {
                    llvm::LLVMDIBuilderCreateEnumerator(
                        DIB(cx),
                        name,
                        v.disr_val as c_ulonglong)
                }
            })
        })
        .collect();

    let discriminant_type_metadata = |inttype| {
        // We can reuse the type of the discriminant for all monomorphized
        // instances of an enum because it doesn't depend on any type parameters.
        // The def_id, uniquely identifying the enum's polytype acts as key in
        // this cache.
        let cached_discriminant_type_metadata = debug_context(cx).created_enum_disr_types
                                                                 .borrow()
                                                                 .find_copy(&enum_def_id);
        match cached_discriminant_type_metadata {
            Some(discriminant_type_metadata) => discriminant_type_metadata,
            None => {
                let discriminant_llvm_type = adt::ll_inttype(cx, inttype);
                let (discriminant_size, discriminant_align) =
                    size_and_align_of(cx, discriminant_llvm_type);
                let discriminant_base_type_metadata = type_metadata(cx,
                                                                    adt::ty_of_inttype(inttype),
                                                                    codemap::DUMMY_SP);
                let discriminant_name = get_enum_discriminant_name(cx, enum_def_id);

                let discriminant_type_metadata = discriminant_name.get().with_c_str(|name| {
                    unsafe {
                        llvm::LLVMDIBuilderCreateEnumerationType(
                            DIB(cx),
                            containing_scope,
                            name,
                            UNKNOWN_FILE_METADATA,
                            UNKNOWN_LINE_NUMBER,
                            bytes_to_bits(discriminant_size),
                            bytes_to_bits(discriminant_align),
                            create_DIArray(DIB(cx), enumerators_metadata.as_slice()),
                            discriminant_base_type_metadata)
                    }
                });

                debug_context(cx).created_enum_disr_types
                                 .borrow_mut()
                                 .insert(enum_def_id, discriminant_type_metadata);

                discriminant_type_metadata
            }
        }
    };

    let type_rep = adt::represent_type(cx, enum_type);

    let discriminant_type_metadata = match *type_rep {
        adt::CEnum(inttype, _, _) => {
            return FinalMetadata(discriminant_type_metadata(inttype))
        },
        adt::RawNullablePointer { .. }           |
        adt::StructWrappedNullablePointer { .. } |
        adt::Univariant(..)                      => None,
        adt::General(inttype, _, _) => Some(discriminant_type_metadata(inttype)),
    };

    let enum_llvm_type = type_of::type_of(cx, enum_type);
    let (enum_type_size, enum_type_align) = size_and_align_of(cx, enum_llvm_type);

    let unique_type_id_str = debug_context(cx)
                             .type_map
                             .borrow()
                             .get_unique_type_id_as_string(unique_type_id);

    let enum_metadata = enum_name.as_slice().with_c_str(|enum_name| {
        unique_type_id_str.as_slice().with_c_str(|unique_type_id_str| {
            unsafe {
                llvm::LLVMDIBuilderCreateUnionType(
                DIB(cx),
                containing_scope,
                enum_name,
                UNKNOWN_FILE_METADATA,
                UNKNOWN_LINE_NUMBER,
                bytes_to_bits(enum_type_size),
                bytes_to_bits(enum_type_align),
                0, // Flags
                ptr::mut_null(),
                0, // RuntimeLang
                unique_type_id_str)
            }
        })
    });

    return create_and_register_recursive_type_forward_declaration(
        cx,
        enum_type,
        unique_type_id,
        enum_metadata,
        enum_llvm_type,
        EnumMDF(EnumMemberDescriptionFactory {
            enum_type: enum_type,
            type_rep: type_rep.clone(),
            variants: variants,
            discriminant_type_metadata: discriminant_type_metadata,
            containing_scope: containing_scope,
            file_metadata: file_metadata,
            span: span,
        }),
    );

    fn get_enum_discriminant_name(cx: &CrateContext,
                                  def_id: ast::DefId)
                                  -> token::InternedString {
        let name = if def_id.krate == ast::LOCAL_CRATE {
            cx.tcx.map.get_path_elem(def_id.node).name()
        } else {
            csearch::get_item_path(&cx.tcx, def_id).last().unwrap().name()
        };

        token::get_name(name)
    }
}

/// Creates debug information for a composite type, that is, anything that
/// results in a LLVM struct.
///
/// Examples of Rust types to use this are: structs, tuples, boxes, vecs, and enums.
fn composite_type_metadata(cx: &CrateContext,
                           composite_llvm_type: Type,
                           composite_type_name: &str,
                           composite_type_unique_id: UniqueTypeId,
                           member_descriptions: &[MemberDescription],
                           containing_scope: DIScope,

                           // Ignore source location information as long as it
                           // can't be reconstructed for non-local crates.
                           _file_metadata: DIFile,
                           _definition_span: Span)
                        -> DICompositeType {
    // Create the (empty) struct metadata node ...
    let composite_type_metadata = create_struct_stub(cx,
                                                     composite_llvm_type,
                                                     composite_type_name,
                                                     composite_type_unique_id,
                                                     containing_scope);
    // ... and immediately create and add the member descriptions.
    set_members_of_composite_type(cx,
                                  composite_type_metadata,
                                  composite_llvm_type,
                                  member_descriptions);

    return composite_type_metadata;
}

fn set_members_of_composite_type(cx: &CrateContext,
                                 composite_type_metadata: DICompositeType,
                                 composite_llvm_type: Type,
                                 member_descriptions: &[MemberDescription]) {
    // In some rare cases LLVM metadata uniquing would lead to an existing type
    // description being used instead of a new one created in create_struct_stub.
    // This would cause a hard to trace assertion in DICompositeType::SetTypeArray().
    // The following check makes sure that we get a better error message if this
    // should happen again due to some regression.
    {
        let mut composite_types_completed =
            debug_context(cx).composite_types_completed.borrow_mut();
        if composite_types_completed.contains(&composite_type_metadata) {
            let (llvm_version_major, llvm_version_minor) = unsafe {
                (llvm::LLVMVersionMajor(), llvm::LLVMVersionMinor())
            };

            let actual_llvm_version = llvm_version_major * 1000000 + llvm_version_minor * 1000;
            let min_supported_llvm_version = 3 * 1000000 + 4 * 1000;

            if actual_llvm_version < min_supported_llvm_version {
                cx.sess().warn(format!("This version of rustc was built with LLVM \
                                        {}.{}. Rustc just ran into a known \
                                        debuginfo corruption problem thatoften \
                                        occurs with LLVM versions below 3.4. \
                                        Please use a rustc built with anewer \
                                        version of LLVM.",
                                       llvm_version_major,
                                       llvm_version_minor).as_slice());
            } else {
                cx.sess().bug("debuginfo::set_members_of_composite_type() - \
                               Already completed forward declaration re-encountered.");
            }
        } else {
            composite_types_completed.insert(composite_type_metadata);
        }
    }

    let member_metadata: Vec<DIDescriptor> = member_descriptions
        .iter()
        .enumerate()
        .map(|(i, member_description)| {
            let (member_size, member_align) = size_and_align_of(cx, member_description.llvm_type);
            let member_offset = match member_description.offset {
                FixedMemberOffset { bytes } => bytes as u64,
                ComputedMemberOffset => machine::llelement_offset(cx, composite_llvm_type, i)
            };

            member_description.name.as_slice().with_c_str(|member_name| {
                unsafe {
                    llvm::LLVMDIBuilderCreateMemberType(
                        DIB(cx),
                        composite_type_metadata,
                        member_name,
                        UNKNOWN_FILE_METADATA,
                        UNKNOWN_LINE_NUMBER,
                        bytes_to_bits(member_size),
                        bytes_to_bits(member_align),
                        bytes_to_bits(member_offset),
                        member_description.flags,
                        member_description.type_metadata)
                }
            })
        })
        .collect();

    unsafe {
        let type_array = create_DIArray(DIB(cx), member_metadata.as_slice());
        llvm::LLVMDICompositeTypeSetTypeArray(composite_type_metadata, type_array);
    }
}

// A convenience wrapper around LLVMDIBuilderCreateStructType(). Does not do any
// caching, does not add any fields to the struct. This can be done later with
// set_members_of_composite_type().
fn create_struct_stub(cx: &CrateContext,
                      struct_llvm_type: Type,
                      struct_type_name: &str,
                      unique_type_id: UniqueTypeId,
                      containing_scope: DIScope)
                   -> DICompositeType {
    let (struct_size, struct_align) = size_and_align_of(cx, struct_llvm_type);

    let unique_type_id_str = debug_context(cx).type_map
                                              .borrow()
                                              .get_unique_type_id_as_string(unique_type_id);
    let metadata_stub = unsafe {
        struct_type_name.with_c_str(|name| {
            unique_type_id_str.as_slice().with_c_str(|unique_type_id| {
                // LLVMDIBuilderCreateStructType() wants an empty array. A null
                // pointer will lead to hard to trace and debug LLVM assertions
                // later on in llvm/lib/IR/Value.cpp.
                let empty_array = create_DIArray(DIB(cx), []);

                llvm::LLVMDIBuilderCreateStructType(
                    DIB(cx),
                    containing_scope,
                    name,
                    UNKNOWN_FILE_METADATA,
                    UNKNOWN_LINE_NUMBER,
                    bytes_to_bits(struct_size),
                    bytes_to_bits(struct_align),
                    0,
                    ptr::mut_null(),
                    empty_array,
                    0,
                    ptr::mut_null(),
                    unique_type_id)
            })
        })
    };

    return metadata_stub;
}

fn at_box_metadata(cx: &CrateContext,
                   at_pointer_type: ty::t,
                   content_type: ty::t,
                   unique_type_id: UniqueTypeId)
                -> MetadataCreationResult {
    let content_type_metadata = type_metadata(cx, content_type, codemap::DUMMY_SP);

    return_if_metadata_created_in_meantime!(cx, unique_type_id);

    let content_type_name = compute_debuginfo_type_name(cx, content_type, true);
    let content_type_name = content_type_name.as_slice();
    let content_llvm_type = type_of::type_of(cx, content_type);

    let box_type_name = format!("GcBox<{}>", content_type_name);
    let box_llvm_type = Type::at_box(cx, content_llvm_type);
    let member_llvm_types = box_llvm_type.field_types();
    assert!(box_layout_is_correct(cx,
                                  member_llvm_types.as_slice(),
                                  content_llvm_type));

    let int_type = ty::mk_int();
    let nil_pointer_type = ty::mk_nil_ptr(cx.tcx());
    let nil_pointer_type_metadata = type_metadata(cx,
                                                  nil_pointer_type,
                                                  codemap::DUMMY_SP);
    let member_descriptions = [
        MemberDescription {
            name: "refcnt".to_string(),
            llvm_type: *member_llvm_types.get(0),
            type_metadata: type_metadata(cx, int_type, codemap::DUMMY_SP),
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL,
        },
        MemberDescription {
            name: "drop_glue".to_string(),
            llvm_type: *member_llvm_types.get(1),
            type_metadata: nil_pointer_type_metadata,
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL,
        },
        MemberDescription {
            name: "prev".to_string(),
            llvm_type: *member_llvm_types.get(2),
            type_metadata: nil_pointer_type_metadata,
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL,
        },
        MemberDescription {
            name: "next".to_string(),
            llvm_type: *member_llvm_types.get(3),
            type_metadata: nil_pointer_type_metadata,
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL,
        },
        MemberDescription {
            name: "val".to_string(),
            llvm_type: *member_llvm_types.get(4),
            type_metadata: content_type_metadata,
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL,
        }
    ];

    let gc_box_unique_id = debug_context(cx).type_map
                                            .borrow_mut()
                                            .get_unique_type_id_of_gc_box(cx, content_type);

    let gc_box_metadata = composite_type_metadata(
        cx,
        box_llvm_type,
        box_type_name.as_slice(),
        gc_box_unique_id,
        member_descriptions,
        UNKNOWN_SCOPE_METADATA,
        UNKNOWN_FILE_METADATA,
        codemap::DUMMY_SP);

    let gc_pointer_metadata = pointer_type_metadata(cx,
                                                    at_pointer_type,
                                                    gc_box_metadata);

    return MetadataCreationResult::new(gc_pointer_metadata, false);

    // Unfortunately, we cannot assert anything but the correct types here---and
    // not whether the 'next' and 'prev' pointers are in the correct order.
    fn box_layout_is_correct(cx: &CrateContext,
                             member_llvm_types: &[Type],
                             content_llvm_type: Type)
                          -> bool {
        member_llvm_types.len() == 5 &&
        member_llvm_types[0] == cx.int_type &&
        member_llvm_types[1] == Type::generic_glue_fn(cx).ptr_to() &&
        member_llvm_types[2] == Type::i8(cx).ptr_to() &&
        member_llvm_types[3] == Type::i8(cx).ptr_to() &&
        member_llvm_types[4] == content_llvm_type
    }
}


fn fixed_vec_metadata(cx: &CrateContext,
                      unique_type_id: UniqueTypeId,
                      element_type: ty::t,
                      len: uint,
                      span: Span)
                   -> MetadataCreationResult {
    let element_type_metadata = type_metadata(cx, element_type, span);

    return_if_metadata_created_in_meantime!(cx, unique_type_id);

    let element_llvm_type = type_of::type_of(cx, element_type);
    let (element_type_size, element_type_align) = size_and_align_of(cx, element_llvm_type);

    let subrange = unsafe {
        llvm::LLVMDIBuilderGetOrCreateSubrange(
        DIB(cx),
        0,
        len as c_longlong)
    };

    let subscripts = create_DIArray(DIB(cx), [subrange]);
    let metadata = unsafe {
        llvm::LLVMDIBuilderCreateArrayType(
            DIB(cx),
            bytes_to_bits(element_type_size * (len as u64)),
            bytes_to_bits(element_type_align),
            element_type_metadata,
            subscripts)
    };

    return MetadataCreationResult::new(metadata, false);
}

fn vec_slice_metadata(cx: &CrateContext,
                      vec_type: ty::t,
                      element_type: ty::t,
                      unique_type_id: UniqueTypeId,
                      span: Span)
                   -> MetadataCreationResult {
    let data_ptr_type = ty::mk_ptr(cx.tcx(), ty::mt {
        ty: element_type,
        mutbl: ast::MutImmutable
    });

    let element_type_metadata = type_metadata(cx, data_ptr_type, span);

    return_if_metadata_created_in_meantime!(cx, unique_type_id);

    let slice_llvm_type = type_of::type_of(cx, vec_type);
    let slice_type_name = compute_debuginfo_type_name(cx, vec_type, true);

    let member_llvm_types = slice_llvm_type.field_types();
    assert!(slice_layout_is_correct(cx,
                                    member_llvm_types.as_slice(),
                                    element_type));
    let member_descriptions = [
        MemberDescription {
            name: "data_ptr".to_string(),
            llvm_type: *member_llvm_types.get(0),
            type_metadata: element_type_metadata,
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL
        },
        MemberDescription {
            name: "length".to_string(),
            llvm_type: *member_llvm_types.get(1),
            type_metadata: type_metadata(cx, ty::mk_uint(), span),
            offset: ComputedMemberOffset,
            flags: FLAGS_ARTIFICAL
        },
    ];

    assert!(member_descriptions.len() == member_llvm_types.len());

    let loc = span_start(cx, span);
    let file_metadata = file_metadata(cx, loc.file.name.as_slice());

    let metadata = composite_type_metadata(cx,
                                           slice_llvm_type,
                                           slice_type_name.as_slice(),
                                           unique_type_id,
                                           member_descriptions,
                                           UNKNOWN_SCOPE_METADATA,
                                           file_metadata,
                                           span);
    return MetadataCreationResult::new(metadata, false);

    fn slice_layout_is_correct(cx: &CrateContext,
                               member_llvm_types: &[Type],
                               element_type: ty::t)
                            -> bool {
        member_llvm_types.len() == 2 &&
        member_llvm_types[0] == type_of::type_of(cx, element_type).ptr_to() &&
        member_llvm_types[1] == cx.int_type
    }
}

fn subroutine_type_metadata(cx: &CrateContext,
                            unique_type_id: UniqueTypeId,
                            signature: &ty::FnSig,
                            span: Span)
                         -> MetadataCreationResult {
    let mut signature_metadata: Vec<DIType> = Vec::with_capacity(signature.inputs.len() + 1);

    // return type
    signature_metadata.push(match ty::get(signature.output).sty {
        ty::ty_nil => ptr::mut_null(),
        _ => type_metadata(cx, signature.output, span)
    });

    // regular arguments
    for &argument_type in signature.inputs.iter() {
        signature_metadata.push(type_metadata(cx, argument_type, span));
    }

    return_if_metadata_created_in_meantime!(cx, unique_type_id);

    return MetadataCreationResult::new(
        unsafe {
            llvm::LLVMDIBuilderCreateSubroutineType(
                DIB(cx),
                UNKNOWN_FILE_METADATA,
                create_DIArray(DIB(cx), signature_metadata.as_slice()))
        },
        false);
}

// FIXME(1563) This is all a bit of a hack because 'trait pointer' is an ill-
// defined concept. For the case of an actual trait pointer (i.e., Box<Trait>,
// &Trait), trait_object_type should be the whole thing (e.g, Box<Trait>) and
// trait_type should be the actual trait (e.g., Trait). Where the trait is part
// of a DST struct, there is no trait_object_type and the results of this
// function will be a little bit weird.
fn trait_pointer_metadata(cx: &CrateContext,
                          trait_type: ty::t,
                          trait_object_type: Option<ty::t>,
                          unique_type_id: UniqueTypeId)
                       -> DIType {
    // The implementation provided here is a stub. It makes sure that the trait
    // type is assigned the correct name, size, namespace, and source location.
    // But it does not describe the trait's methods.

    let def_id = match ty::get(trait_type).sty {
        ty::ty_trait(box ty::TyTrait { def_id, .. }) => def_id,
        _ => {
            let pp_type_name = ppaux::ty_to_string(cx.tcx(), trait_type);
            cx.sess().bug(format!("debuginfo: Unexpected trait-object type in \
                                   trait_pointer_metadata(): {}",
                                   pp_type_name.as_slice()).as_slice());
        }
    };

    let trait_object_type = trait_object_type.unwrap_or(trait_type);
    let trait_type_name =
        compute_debuginfo_type_name(cx, trait_object_type, false);

    let (containing_scope, _) = get_namespace_and_span_for_item(cx, def_id);

    let trait_llvm_type = type_of::type_of(cx, trait_object_type);

    composite_type_metadata(cx,
                            trait_llvm_type,
                            trait_type_name.as_slice(),
                            unique_type_id,
                            [],
                            containing_scope,
                            UNKNOWN_FILE_METADATA,
                            codemap::DUMMY_SP)
}

fn type_metadata(cx: &CrateContext,
                 t: ty::t,
                 usage_site_span: Span)
              -> DIType {
    // Get the unique type id of this type.
    let unique_type_id = {
        let mut type_map = debug_context(cx).type_map.borrow_mut();
        // First, try to find the type in TypeMap. If we have seen it before, we
        // can exit early here.
        match type_map.find_metadata_for_type(t) {
            Some(metadata) => {
                return metadata;
            },
            None => {
                // The ty::t is not in the TypeMap but maybe we have already seen
                // an equivalent type (e.g. only differing in region arguments).
                // In order to find out, generate the unique type id and look
                // that up.
                let unique_type_id = type_map.get_unique_type_id_of_type(cx, t);
                match type_map.find_metadata_for_unique_id(unique_type_id) {
                    Some(metadata) => {
                        // There is already an equivalent type in the TypeMap.
                        // Register this ty::t as an alias in the cache and
                        // return the cached metadata.
                        type_map.register_type_with_metadata(cx, t, metadata);
                        return metadata;
                    },
                    None => {
                        // There really is no type metadata for this type, so
                        // proceed by creating it.
                        unique_type_id
                    }
                }
            }
        }
    };

    debug!("type_metadata: {:?}", ty::get(t));

    let sty = &ty::get(t).sty;
    let MetadataCreationResult { metadata, already_stored_in_typemap } = match *sty {
        ty::ty_nil      |
        ty::ty_bot      |
        ty::ty_bool     |
        ty::ty_char     |
        ty::ty_int(_)   |
        ty::ty_uint(_)  |
        ty::ty_float(_) => {
            MetadataCreationResult::new(basic_type_metadata(cx, t), false)
        }
        ty::ty_enum(def_id, _) => {
            prepare_enum_metadata(cx, t, def_id, unique_type_id, usage_site_span).finalize(cx)
        }
        ty::ty_box(pointee_type) => {
            at_box_metadata(cx, t, pointee_type, unique_type_id)
        }
        ty::ty_vec(typ, Some(len)) => {
            fixed_vec_metadata(cx, unique_type_id, typ, len, usage_site_span)
        }
        // FIXME Can we do better than this for unsized vec/str fields?
        ty::ty_vec(typ, None) => fixed_vec_metadata(cx, unique_type_id, typ, 0, usage_site_span),
        ty::ty_str => fixed_vec_metadata(cx, unique_type_id, ty::mk_i8(), 0, usage_site_span),
        ty::ty_trait(..) => {
            MetadataCreationResult::new(
                        trait_pointer_metadata(cx, t, None, unique_type_id),
            false)
        }
        ty::ty_uniq(ty) | ty::ty_ptr(ty::mt{ty, ..}) | ty::ty_rptr(_, ty::mt{ty, ..}) => {
            match ty::get(ty).sty {
                ty::ty_vec(typ, None) => {
                    vec_slice_metadata(cx, t, typ, unique_type_id, usage_site_span)
                }
                ty::ty_str => {
                    vec_slice_metadata(cx, t, ty::mk_u8(), unique_type_id, usage_site_span)
                }
                ty::ty_trait(..) => {
                    MetadataCreationResult::new(
                        trait_pointer_metadata(cx, ty, Some(t), unique_type_id),
                        false)
                }
                _ => {
                    let pointee_metadata = type_metadata(cx, ty, usage_site_span);

                    match debug_context(cx).type_map
                                           .borrow()
                                           .find_metadata_for_unique_id(unique_type_id) {
                        Some(metadata) => return metadata,
                        None => { /* proceed normally */ }
                    };

                    MetadataCreationResult::new(pointer_type_metadata(cx, t, pointee_metadata),
                                                false)
                }
            }
        }
        ty::ty_bare_fn(ref barefnty) => {
            subroutine_type_metadata(cx, unique_type_id, &barefnty.sig, usage_site_span)
        }
        ty::ty_closure(ref closurety) => {
            subroutine_type_metadata(cx, unique_type_id, &closurety.sig, usage_site_span)
        }
        ty::ty_struct(def_id, ref substs) => {
            prepare_struct_metadata(cx,
                                    t,
                                    def_id,
                                    substs,
                                    unique_type_id,
                                    usage_site_span).finalize(cx)
        }
        ty::ty_tup(ref elements) => {
            prepare_tuple_metadata(cx,
                                   t,
                                   elements.as_slice(),
                                   unique_type_id,
                                   usage_site_span).finalize(cx)
        }
        _ => {
            cx.sess().bug(format!("debuginfo: unexpected type in type_metadata: {:?}",
                                  sty).as_slice())
        }
    };

    {
        let mut type_map = debug_context(cx).type_map.borrow_mut();

        if already_stored_in_typemap {
            // Also make sure that we already have a TypeMap entry entry for the unique type id.
            let metadata_for_uid = match type_map.find_metadata_for_unique_id(unique_type_id) {
                Some(metadata) => metadata,
                None => {
                    let unique_type_id_str =
                        type_map.get_unique_type_id_as_string(unique_type_id);
                    let error_message = format!("Expected type metadata for unique \
                                                 type id '{}' to already be in \
                                                 the debuginfo::TypeMap but it \
                                                 was not. (ty::t = {})",
                                                unique_type_id_str.as_slice(),
                                                ppaux::ty_to_string(cx.tcx(), t));
                    cx.sess().span_bug(usage_site_span, error_message.as_slice());
                }
            };

            match type_map.find_metadata_for_type(t) {
                Some(metadata) => {
                    if metadata != metadata_for_uid {
                        let unique_type_id_str =
                            type_map.get_unique_type_id_as_string(unique_type_id);
                        let error_message = format!("Mismatch between ty::t and \
                                                     UniqueTypeId maps in \
                                                     debuginfo::TypeMap. \
                                                     UniqueTypeId={}, ty::t={}",
                            unique_type_id_str.as_slice(),
                            ppaux::ty_to_string(cx.tcx(), t));
                        cx.sess().span_bug(usage_site_span, error_message.as_slice());
                    }
                }
                None => {
                    type_map.register_type_with_metadata(cx, t, metadata);
                }
            }
        } else {
            type_map.register_type_with_metadata(cx, t, metadata);
            type_map.register_unique_id_with_metadata(cx, unique_type_id, metadata);
        }
    }

    metadata
}

struct MetadataCreationResult {
    metadata: DIType,
    already_stored_in_typemap: bool
}

impl MetadataCreationResult {
    fn new(metadata: DIType, already_stored_in_typemap: bool) -> MetadataCreationResult {
        MetadataCreationResult {
            metadata: metadata,
            already_stored_in_typemap: already_stored_in_typemap
        }
    }
}

#[deriving(PartialEq)]
enum DebugLocation {
    KnownLocation { scope: DIScope, line: uint, col: uint },
    UnknownLocation
}

impl DebugLocation {
    fn new(scope: DIScope, line: uint, col: uint) -> DebugLocation {
        KnownLocation {
            scope: scope,
            line: line,
            col: col,
        }
    }
}

fn set_debug_location(cx: &CrateContext, debug_location: DebugLocation) {
    if debug_location == debug_context(cx).current_debug_location.get() {
        return;
    }

    let metadata_node;

    match debug_location {
        KnownLocation { scope, line, .. } => {
            // Always set the column to zero like Clang and GCC
            let col = UNKNOWN_COLUMN_NUMBER;
            debug!("setting debug location to {} {}", line, col);
            let elements = [C_i32(cx, line as i32), C_i32(cx, col as i32),
                            scope, ptr::mut_null()];
            unsafe {
                metadata_node = llvm::LLVMMDNodeInContext(debug_context(cx).llcontext,
                                                          elements.as_ptr(),
                                                          elements.len() as c_uint);
            }
        }
        UnknownLocation => {
            debug!("clearing debug location ");
            metadata_node = ptr::mut_null();
        }
    };

    unsafe {
        llvm::LLVMSetCurrentDebugLocation(cx.builder.b, metadata_node);
    }

    debug_context(cx).current_debug_location.set(debug_location);
}

//=-----------------------------------------------------------------------------
//  Utility Functions
//=-----------------------------------------------------------------------------

fn contains_nodebug_attribute(attributes: &[ast::Attribute]) -> bool {
    attributes.iter().any(|attr| {
        let meta_item: &ast::MetaItem = &*attr.node.value;
        match meta_item.node {
            ast::MetaWord(ref value) => value.get() == "no_debug",
            _ => false
        }
    })
}

/// Return codemap::Loc corresponding to the beginning of the span
fn span_start(cx: &CrateContext, span: Span) -> codemap::Loc {
    cx.sess().codemap().lookup_char_pos(span.lo)
}

fn size_and_align_of(cx: &CrateContext, llvm_type: Type) -> (u64, u64) {
    (machine::llsize_of_alloc(cx, llvm_type), machine::llalign_of_min(cx, llvm_type))
}

fn bytes_to_bits(bytes: u64) -> c_ulonglong {
    (bytes * 8) as c_ulonglong
}

#[inline]
fn debug_context<'a>(cx: &'a CrateContext) -> &'a CrateDebugContext {
    let debug_context: &'a CrateDebugContext = cx.dbg_cx.get_ref();
    debug_context
}

#[inline]
#[allow(non_snake_case)]
fn DIB(cx: &CrateContext) -> DIBuilderRef {
    cx.dbg_cx.get_ref().builder
}

fn fn_should_be_ignored(fcx: &FunctionContext) -> bool {
    match fcx.debug_context.repr {
        FunctionDebugContext(_) => false,
        _ => true
    }
}

fn assert_type_for_node_id(cx: &CrateContext, node_id: ast::NodeId, error_span: Span) {
    if !cx.tcx.node_types.borrow().contains_key(&(node_id as uint)) {
        cx.sess().span_bug(error_span, "debuginfo: Could not find type for node id!");
    }
}

fn get_namespace_and_span_for_item(cx: &CrateContext, def_id: ast::DefId)
                                   -> (DIScope, Span) {
    let containing_scope = namespace_for_item(cx, def_id).scope;
    let definition_span = if def_id.krate == ast::LOCAL_CRATE {
        cx.tcx.map.span(def_id.node)
    } else {
        // For external items there is no span information
        codemap::DUMMY_SP
    };

    (containing_scope, definition_span)
}

// This procedure builds the *scope map* for a given function, which maps any
// given ast::NodeId in the function's AST to the correct DIScope metadata instance.
//
// This builder procedure walks the AST in execution order and keeps track of
// what belongs to which scope, creating DIScope DIEs along the way, and
// introducing *artificial* lexical scope descriptors where necessary. These
// artificial scopes allow GDB to correctly handle name shadowing.
fn populate_scope_map(cx: &CrateContext,
                      arg_pats: &[Gc<ast::Pat>],
                      fn_entry_block: &ast::Block,
                      fn_metadata: DISubprogram,
                      scope_map: &mut HashMap<ast::NodeId, DIScope>) {
    let def_map = &cx.tcx.def_map;

    struct ScopeStackEntry {
        scope_metadata: DIScope,
        ident: Option<ast::Ident>
    }

    let mut scope_stack = vec!(ScopeStackEntry { scope_metadata: fn_metadata,
                                                 ident: None });

    // Push argument identifiers onto the stack so arguments integrate nicely
    // with variable shadowing.
    for &arg_pat in arg_pats.iter() {
        pat_util::pat_bindings(def_map, &*arg_pat, |_, _, _, path1| {
            scope_stack.push(ScopeStackEntry { scope_metadata: fn_metadata,
                                               ident: Some(path1.node) });
        })
    }

    // Clang creates a separate scope for function bodies, so let's do this too.
    with_new_scope(cx,
                   fn_entry_block.span,
                   &mut scope_stack,
                   scope_map,
                   |cx, scope_stack, scope_map| {
        walk_block(cx, fn_entry_block, scope_stack, scope_map);
    });

    // local helper functions for walking the AST.
    fn with_new_scope(cx: &CrateContext,
                      scope_span: Span,
                      scope_stack: &mut Vec<ScopeStackEntry> ,
                      scope_map: &mut HashMap<ast::NodeId, DIScope>,
                      inner_walk: |&CrateContext,
                                   &mut Vec<ScopeStackEntry> ,
                                   &mut HashMap<ast::NodeId, DIScope>|) {
        // Create a new lexical scope and push it onto the stack
        let loc = cx.sess().codemap().lookup_char_pos(scope_span.lo);
        let file_metadata = file_metadata(cx, loc.file.name.as_slice());
        let parent_scope = scope_stack.last().unwrap().scope_metadata;

        let scope_metadata = unsafe {
            llvm::LLVMDIBuilderCreateLexicalBlock(
                DIB(cx),
                parent_scope,
                file_metadata,
                loc.line as c_uint,
                loc.col.to_uint() as c_uint,
                0)
        };

        scope_stack.push(ScopeStackEntry { scope_metadata: scope_metadata,
                                           ident: None });

        inner_walk(cx, scope_stack, scope_map);

        // pop artificial scopes
        while scope_stack.last().unwrap().ident.is_some() {
            scope_stack.pop();
        }

        if scope_stack.last().unwrap().scope_metadata != scope_metadata {
            cx.sess().span_bug(scope_span, "debuginfo: Inconsistency in scope management.");
        }

        scope_stack.pop();
    }

    fn walk_block(cx: &CrateContext,
                  block: &ast::Block,
                  scope_stack: &mut Vec<ScopeStackEntry> ,
                  scope_map: &mut HashMap<ast::NodeId, DIScope>) {
        scope_map.insert(block.id, scope_stack.last().unwrap().scope_metadata);

        // The interesting things here are statements and the concluding expression.
        for statement in block.stmts.iter() {
            scope_map.insert(ast_util::stmt_id(&**statement),
                             scope_stack.last().unwrap().scope_metadata);

            match statement.node {
                ast::StmtDecl(ref decl, _) =>
                    walk_decl(cx, &**decl, scope_stack, scope_map),
                ast::StmtExpr(ref exp, _) |
                ast::StmtSemi(ref exp, _) =>
                    walk_expr(cx, &**exp, scope_stack, scope_map),
                ast::StmtMac(..) => () // Ignore macros (which should be expanded anyway).
            }
        }

        for exp in block.expr.iter() {
            walk_expr(cx, &**exp, scope_stack, scope_map);
        }
    }

    fn walk_decl(cx: &CrateContext,
                 decl: &ast::Decl,
                 scope_stack: &mut Vec<ScopeStackEntry> ,
                 scope_map: &mut HashMap<ast::NodeId, DIScope>) {
        match *decl {
            codemap::Spanned { node: ast::DeclLocal(local), .. } => {
                scope_map.insert(local.id, scope_stack.last().unwrap().scope_metadata);

                walk_pattern(cx, local.pat, scope_stack, scope_map);

                for exp in local.init.iter() {
                    walk_expr(cx, &**exp, scope_stack, scope_map);
                }
            }
            _ => ()
        }
    }

    fn walk_pattern(cx: &CrateContext,
                    pat: Gc<ast::Pat>,
                    scope_stack: &mut Vec<ScopeStackEntry> ,
                    scope_map: &mut HashMap<ast::NodeId, DIScope>) {

        let def_map = &cx.tcx.def_map;

        // Unfortunately, we cannot just use pat_util::pat_bindings() or
        // ast_util::walk_pat() here because we have to visit *all* nodes in
        // order to put them into the scope map. The above functions don't do that.
        match pat.node {
            ast::PatIdent(_, ref path1, ref sub_pat_opt) => {

                // Check if this is a binding. If so we need to put it on the
                // scope stack and maybe introduce an artificial scope
                if pat_util::pat_is_binding(def_map, &*pat) {

                    let ident = path1.node;

                    // LLVM does not properly generate 'DW_AT_start_scope' fields
                    // for variable DIEs. For this reason we have to introduce
                    // an artificial scope at bindings whenever a variable with
                    // the same name is declared in *any* parent scope.
                    //
                    // Otherwise the following error occurs:
                    //
                    // let x = 10;
                    //
                    // do_something(); // 'gdb print x' correctly prints 10
                    //
                    // {
                    //     do_something(); // 'gdb print x' prints 0, because it
                    //                     // already reads the uninitialized 'x'
                    //                     // from the next line...
                    //     let x = 100;
                    //     do_something(); // 'gdb print x' correctly prints 100
                    // }

                    // Is there already a binding with that name?
                    // N.B.: this comparison must be UNhygienic... because
                    // gdb knows nothing about the context, so any two
                    // variables with the same name will cause the problem.
                    let need_new_scope = scope_stack
                        .iter()
                        .any(|entry| entry.ident.iter().any(|i| i.name == ident.name));

                    if need_new_scope {
                        // Create a new lexical scope and push it onto the stack
                        let loc = cx.sess().codemap().lookup_char_pos(pat.span.lo);
                        let file_metadata = file_metadata(cx,
                                                          loc.file
                                                             .name
                                                             .as_slice());
                        let parent_scope = scope_stack.last().unwrap().scope_metadata;

                        let scope_metadata = unsafe {
                            llvm::LLVMDIBuilderCreateLexicalBlock(
                                DIB(cx),
                                parent_scope,
                                file_metadata,
                                loc.line as c_uint,
                                loc.col.to_uint() as c_uint,
                                0)
                        };

                        scope_stack.push(ScopeStackEntry {
                            scope_metadata: scope_metadata,
                            ident: Some(ident)
                        });

                    } else {
                        // Push a new entry anyway so the name can be found
                        let prev_metadata = scope_stack.last().unwrap().scope_metadata;
                        scope_stack.push(ScopeStackEntry {
                            scope_metadata: prev_metadata,
                            ident: Some(ident)
                        });
                    }
                }

                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);

                for &sub_pat in sub_pat_opt.iter() {
                    walk_pattern(cx, sub_pat, scope_stack, scope_map);
                }
            }

            ast::PatWild(_) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);
            }

            ast::PatEnum(_, ref sub_pats_opt) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);

                for ref sub_pats in sub_pats_opt.iter() {
                    for &p in sub_pats.iter() {
                        walk_pattern(cx, p, scope_stack, scope_map);
                    }
                }
            }

            ast::PatStruct(_, ref field_pats, _) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);

                for &ast::FieldPat { pat: sub_pat, .. } in field_pats.iter() {
                    walk_pattern(cx, sub_pat, scope_stack, scope_map);
                }
            }

            ast::PatTup(ref sub_pats) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);

                for sub_pat in sub_pats.iter() {
                    walk_pattern(cx, sub_pat.clone(), scope_stack, scope_map);
                }
            }

            ast::PatBox(ref sub_pat) | ast::PatRegion(ref sub_pat) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);
                walk_pattern(cx, sub_pat.clone(), scope_stack, scope_map);
            }

            ast::PatLit(ref exp) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);
                walk_expr(cx, &**exp, scope_stack, scope_map);
            }

            ast::PatRange(ref exp1, ref exp2) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);
                walk_expr(cx, &**exp1, scope_stack, scope_map);
                walk_expr(cx, &**exp2, scope_stack, scope_map);
            }

            ast::PatVec(ref front_sub_pats, ref middle_sub_pats, ref back_sub_pats) => {
                scope_map.insert(pat.id, scope_stack.last().unwrap().scope_metadata);

                for &sub_pat in front_sub_pats.iter() {
                    walk_pattern(cx, sub_pat, scope_stack, scope_map);
                }

                for &sub_pat in middle_sub_pats.iter() {
                    walk_pattern(cx, sub_pat, scope_stack, scope_map);
                }

                for &sub_pat in back_sub_pats.iter() {
                    walk_pattern(cx, sub_pat, scope_stack, scope_map);
                }
            }

            ast::PatMac(_) => {
                cx.sess().span_bug(pat.span, "debuginfo::populate_scope_map() - \
                                              Found unexpanded macro.");
            }
        }
    }

    fn walk_expr(cx: &CrateContext,
                 exp: &ast::Expr,
                 scope_stack: &mut Vec<ScopeStackEntry> ,
                 scope_map: &mut HashMap<ast::NodeId, DIScope>) {

        scope_map.insert(exp.id, scope_stack.last().unwrap().scope_metadata);

        match exp.node {
            ast::ExprLit(_)   |
            ast::ExprBreak(_) |
            ast::ExprAgain(_) |
            ast::ExprPath(_)  => {}

            ast::ExprCast(ref sub_exp, _)     |
            ast::ExprAddrOf(_, ref sub_exp)  |
            ast::ExprField(ref sub_exp, _, _) |
            ast::ExprParen(ref sub_exp) =>
                walk_expr(cx, &**sub_exp, scope_stack, scope_map),

            ast::ExprBox(ref place, ref sub_expr) => {
                walk_expr(cx, &**place, scope_stack, scope_map);
                walk_expr(cx, &**sub_expr, scope_stack, scope_map);
            }

            ast::ExprRet(exp_opt) => match exp_opt {
                Some(sub_exp) => walk_expr(cx, &*sub_exp, scope_stack, scope_map),
                None => ()
            },

            ast::ExprUnary(_, ref sub_exp) => {
                walk_expr(cx, &**sub_exp, scope_stack, scope_map);
            }

            ast::ExprAssignOp(_, ref lhs, ref rhs) |
            ast::ExprIndex(ref lhs, ref rhs)        |
            ast::ExprBinary(_, ref lhs, ref rhs)    => {
                walk_expr(cx, &**lhs, scope_stack, scope_map);
                walk_expr(cx, &**rhs, scope_stack, scope_map);
            }

            ast::ExprVec(ref init_expressions) |
            ast::ExprTup(ref init_expressions) => {
                for ie in init_expressions.iter() {
                    walk_expr(cx, &**ie, scope_stack, scope_map);
                }
            }

            ast::ExprAssign(ref sub_exp1, ref sub_exp2) |
            ast::ExprRepeat(ref sub_exp1, ref sub_exp2) => {
                walk_expr(cx, &**sub_exp1, scope_stack, scope_map);
                walk_expr(cx, &**sub_exp2, scope_stack, scope_map);
            }

            ast::ExprIf(ref cond_exp, ref then_block, ref opt_else_exp) => {
                walk_expr(cx, &**cond_exp, scope_stack, scope_map);

                with_new_scope(cx,
                               then_block.span,
                               scope_stack,
                               scope_map,
                               |cx, scope_stack, scope_map| {
                    walk_block(cx, &**then_block, scope_stack, scope_map);
                });

                match *opt_else_exp {
                    Some(ref else_exp) =>
                        walk_expr(cx, &**else_exp, scope_stack, scope_map),
                    _ => ()
                }
            }

            ast::ExprWhile(ref cond_exp, ref loop_body, _) => {
                walk_expr(cx, &**cond_exp, scope_stack, scope_map);

                with_new_scope(cx,
                               loop_body.span,
                               scope_stack,
                               scope_map,
                               |cx, scope_stack, scope_map| {
                    walk_block(cx, &**loop_body, scope_stack, scope_map);
                })
            }

            ast::ExprForLoop(ref pattern, ref head, ref body, _) => {
                walk_expr(cx, &**head, scope_stack, scope_map);

                with_new_scope(cx,
                               exp.span,
                               scope_stack,
                               scope_map,
                               |cx, scope_stack, scope_map| {
                    scope_map.insert(exp.id,
                                     scope_stack.last()
                                                .unwrap()
                                                .scope_metadata);
                    walk_pattern(cx,
                                 *pattern,
                                 scope_stack,
                                 scope_map);
                    walk_block(cx, &**body, scope_stack, scope_map);
                })
            }

            ast::ExprMac(_) => {
                cx.sess().span_bug(exp.span, "debuginfo::populate_scope_map() - \
                                              Found unexpanded macro.");
            }

            ast::ExprLoop(ref block, _) |
            ast::ExprBlock(ref block)   => {
                with_new_scope(cx,
                               block.span,
                               scope_stack,
                               scope_map,
                               |cx, scope_stack, scope_map| {
                    walk_block(cx, &**block, scope_stack, scope_map);
                })
            }

            ast::ExprFnBlock(_, ref decl, ref block) |
            ast::ExprProc(ref decl, ref block) |
            ast::ExprUnboxedFn(_, _, ref decl, ref block) => {
                with_new_scope(cx,
                               block.span,
                               scope_stack,
                               scope_map,
                               |cx, scope_stack, scope_map| {
                    for &ast::Arg { pat: ref pattern, .. } in decl.inputs.iter() {
                        walk_pattern(cx, pattern.clone(), scope_stack, scope_map);
                    }

                    walk_block(cx, &**block, scope_stack, scope_map);
                })
            }

            ast::ExprCall(ref fn_exp, ref args) => {
                walk_expr(cx, &**fn_exp, scope_stack, scope_map);

                for arg_exp in args.iter() {
                    walk_expr(cx, &**arg_exp, scope_stack, scope_map);
                }
            }

            ast::ExprMethodCall(_, _, ref args) => {
                for arg_exp in args.iter() {
                    walk_expr(cx, &**arg_exp, scope_stack, scope_map);
                }
            }

            ast::ExprMatch(ref discriminant_exp, ref arms) => {
                walk_expr(cx, &**discriminant_exp, scope_stack, scope_map);

                // For each arm we have to first walk the pattern as these might
                // introduce new artificial scopes. It should be sufficient to
                // walk only one pattern per arm, as they all must contain the
                // same binding names.

                for arm_ref in arms.iter() {
                    let arm_span = arm_ref.pats.get(0).span;

                    with_new_scope(cx,
                                   arm_span,
                                   scope_stack,
                                   scope_map,
                                   |cx, scope_stack, scope_map| {
                        for &pat in arm_ref.pats.iter() {
                            walk_pattern(cx, pat, scope_stack, scope_map);
                        }

                        for guard_exp in arm_ref.guard.iter() {
                            walk_expr(cx, &**guard_exp, scope_stack, scope_map)
                        }

                        walk_expr(cx, &*arm_ref.body, scope_stack, scope_map);
                    })
                }
            }

            ast::ExprStruct(_, ref fields, ref base_exp) => {
                for &ast::Field { expr: ref exp, .. } in fields.iter() {
                    walk_expr(cx, &**exp, scope_stack, scope_map);
                }

                match *base_exp {
                    Some(ref exp) => walk_expr(cx, &**exp, scope_stack, scope_map),
                    None => ()
                }
            }

            ast::ExprInlineAsm(ast::InlineAsm { inputs: ref inputs,
                                                outputs: ref outputs,
                                                .. }) => {
                // inputs, outputs: ~[(String, Gc<expr>)]
                for &(_, ref exp) in inputs.iter() {
                    walk_expr(cx, &**exp, scope_stack, scope_map);
                }

                for &(_, ref exp, _) in outputs.iter() {
                    walk_expr(cx, &**exp, scope_stack, scope_map);
                }
            }
        }
    }
}


//=-----------------------------------------------------------------------------
// Type Names for Debug Info
//=-----------------------------------------------------------------------------

// Compute the name of the type as it should be stored in debuginfo. Does not do
// any caching, i.e. calling the function twice with the same type will also do
// the work twice. The `qualified` parameter only affects the first level of the
// type name, further levels (i.e. type parameters) are always fully qualified.
fn compute_debuginfo_type_name(cx: &CrateContext,
                               t: ty::t,
                               qualified: bool)
                            -> String {
    let mut result = String::with_capacity(64);
    push_debuginfo_type_name(cx, t, qualified, &mut result);
    result
}

// Pushes the name of the type as it should be stored in debuginfo on the
// `output` String. See also compute_debuginfo_type_name().
fn push_debuginfo_type_name(cx: &CrateContext,
                            t: ty::t,
                            qualified: bool,
                            output:&mut String) {
    match ty::get(t).sty {
        ty::ty_nil               => output.push_str("()"),
        ty::ty_bot               => output.push_str("!"),
        ty::ty_bool              => output.push_str("bool"),
        ty::ty_char              => output.push_str("char"),
        ty::ty_str               => output.push_str("str"),
        ty::ty_int(ast::TyI)     => output.push_str("int"),
        ty::ty_int(ast::TyI8)    => output.push_str("i8"),
        ty::ty_int(ast::TyI16)   => output.push_str("i16"),
        ty::ty_int(ast::TyI32)   => output.push_str("i32"),
        ty::ty_int(ast::TyI64)   => output.push_str("i64"),
        ty::ty_uint(ast::TyU)    => output.push_str("uint"),
        ty::ty_uint(ast::TyU8)   => output.push_str("u8"),
        ty::ty_uint(ast::TyU16)  => output.push_str("u16"),
        ty::ty_uint(ast::TyU32)  => output.push_str("u32"),
        ty::ty_uint(ast::TyU64)  => output.push_str("u64"),
        ty::ty_float(ast::TyF32) => output.push_str("f32"),
        ty::ty_float(ast::TyF64) => output.push_str("f64"),
        ty::ty_struct(def_id, ref substs) |
        ty::ty_enum(def_id, ref substs) => {
            push_item_name(cx, def_id, qualified, output);
            push_type_params(cx, substs, output);
        },
        ty::ty_tup(ref component_types) => {
            output.push_char('(');
            for &component_type in component_types.iter() {
                push_debuginfo_type_name(cx, component_type, true, output);
                output.push_str(", ");
            }
            output.pop_char();
            output.pop_char();
            output.push_char(')');
        },
        ty::ty_uniq(inner_type) => {
            output.push_str("Box<");
            push_debuginfo_type_name(cx, inner_type, true, output);
            output.push_char('>');
        },
        ty::ty_box(inner_type) => {
            output.push_char('@');
            push_debuginfo_type_name(cx, inner_type, true, output);
        },
        ty::ty_ptr(ty::mt { ty: inner_type, mutbl } ) => {
            output.push_char('*');
            match mutbl {
                ast::MutImmutable => output.push_str("const "),
                ast::MutMutable => output.push_str("mut "),
            }

            push_debuginfo_type_name(cx, inner_type, true, output);
        },
        ty::ty_rptr(_, ty::mt { ty: inner_type, mutbl }) => {
            output.push_char('&');
            if mutbl == ast::MutMutable {
                output.push_str("mut ");
            }

            push_debuginfo_type_name(cx, inner_type, true, output);
        },
        ty::ty_vec(inner_type, optional_length) => {
            output.push_char('[');
            push_debuginfo_type_name(cx, inner_type, true, output);

            match optional_length {
                Some(len) => {
                    output.push_str(format!(", ..{}", len).as_slice());
                }
                None => { /* nothing to do */ }
            };

            output.push_char(']');
        },
        ty::ty_trait(ref trait_data) => {
            push_item_name(cx, trait_data.def_id, false, output);
            push_type_params(cx, &trait_data.substs, output);
        },
        ty::ty_bare_fn(ty::BareFnTy{ fn_style, abi, ref sig } ) => {
            if fn_style == ast::UnsafeFn {
                output.push_str("unsafe ");
            }

            if abi != ::syntax::abi::Rust {
                output.push_str("extern \"");
                output.push_str(abi.name());
                output.push_str("\" ");
            }

            output.push_str("fn(");

            if sig.inputs.len() > 0 {
                for &parameter_type in sig.inputs.iter() {
                    push_debuginfo_type_name(cx, parameter_type, true, output);
                    output.push_str(", ");
                }
                output.pop_char();
                output.pop_char();
            }

            if sig.variadic {
                if sig.inputs.len() > 0 {
                    output.push_str(", ...");
                } else {
                    output.push_str("...");
                }
            }

            output.push_char(')');

            if !ty::type_is_nil(sig.output) {
                output.push_str(" -> ");
                push_debuginfo_type_name(cx, sig.output, true, output);
            }
        },
        ty::ty_closure(box ty::ClosureTy { fn_style,
                                           onceness,
                                           store,
                                           ref sig,
                                           .. // omitting bounds ...
                                           }) => {
            if fn_style == ast::UnsafeFn {
                output.push_str("unsafe ");
            }

            if onceness == ast::Once {
                output.push_str("once ");
            }

            let param_list_closing_char;
            match store {
                ty::UniqTraitStore => {
                    output.push_str("proc(");
                    param_list_closing_char = ')';
                }
                ty::RegionTraitStore(_, ast::MutMutable) => {
                    output.push_str("&mut|");
                    param_list_closing_char = '|';
                }
                ty::RegionTraitStore(_, ast::MutImmutable) => {
                    output.push_str("&|");
                    param_list_closing_char = '|';
                }
            };

            if sig.inputs.len() > 0 {
                for &parameter_type in sig.inputs.iter() {
                    push_debuginfo_type_name(cx, parameter_type, true, output);
                    output.push_str(", ");
                }
                output.pop_char();
                output.pop_char();
            }

            if sig.variadic {
                if sig.inputs.len() > 0 {
                    output.push_str(", ...");
                } else {
                    output.push_str("...");
                }
            }

            output.push_char(param_list_closing_char);

            if !ty::type_is_nil(sig.output) {
                output.push_str(" -> ");
                push_debuginfo_type_name(cx, sig.output, true, output);
            }
        },
        ty::ty_unboxed_closure(..) => {
            output.push_str("closure");
        }
        ty::ty_err      |
        ty::ty_infer(_) |
        ty::ty_open(_) |
        ty::ty_param(_) => {
            cx.sess().bug(format!("debuginfo: Trying to create type name for \
                unexpected type: {}", ppaux::ty_to_string(cx.tcx(), t)).as_slice());
        }
    }

    fn push_item_name(cx: &CrateContext,
                      def_id: ast::DefId,
                      qualified: bool,
                      output: &mut String) {
        ty::with_path(cx.tcx(), def_id, |mut path| {
            if qualified {
                if def_id.krate == ast::LOCAL_CRATE {
                    output.push_str(crate_root_namespace(cx));
                    output.push_str("::");
                }

                let mut path_element_count = 0u;
                for path_element in path {
                    let name = token::get_name(path_element.name());
                    output.push_str(name.get());
                    output.push_str("::");
                    path_element_count += 1;
                }

                if path_element_count == 0 {
                    cx.sess().bug("debuginfo: Encountered empty item path!");
                }

                output.pop_char();
                output.pop_char();
            } else {
                let name = token::get_name(path.last()
                                               .expect("debuginfo: Empty item path?")
                                               .name());
                output.push_str(name.get());
            }
        });
    }

    // Pushes the type parameters in the given `Substs` to the output string.
    // This ignores region parameters, since they can't reliably be
    // reconstructed for items from non-local crates. For local crates, this
    // would be possible but with inlining and LTO we have to use the least
    // common denominator - otherwise we would run into conflicts.
    fn push_type_params(cx: &CrateContext,
                        substs: &subst::Substs,
                        output: &mut String) {
        if substs.types.is_empty() {
            return;
        }

        output.push_char('<');

        for &type_parameter in substs.types.iter() {
            push_debuginfo_type_name(cx, type_parameter, true, output);
            output.push_str(", ");
        }

        output.pop_char();
        output.pop_char();

        output.push_char('>');
    }
}


//=-----------------------------------------------------------------------------
// Namespace Handling
//=-----------------------------------------------------------------------------

struct NamespaceTreeNode {
    name: ast::Name,
    scope: DIScope,
    parent: Option<Weak<NamespaceTreeNode>>,
}

impl NamespaceTreeNode {
    fn mangled_name_of_contained_item(&self, item_name: &str) -> String {
        fn fill_nested(node: &NamespaceTreeNode, output: &mut String) {
            match node.parent {
                Some(ref parent) => fill_nested(&*parent.upgrade().unwrap(), output),
                None => {}
            }
            let string = token::get_name(node.name);
            output.push_str(format!("{}", string.get().len()).as_slice());
            output.push_str(string.get());
        }

        let mut name = String::from_str("_ZN");
        fill_nested(self, &mut name);
        name.push_str(format!("{}", item_name.len()).as_slice());
        name.push_str(item_name);
        name.push_char('E');
        name
    }
}

fn crate_root_namespace<'a>(cx: &'a CrateContext) -> &'a str {
    cx.link_meta.crate_name.as_slice()
}

fn namespace_for_item(cx: &CrateContext, def_id: ast::DefId) -> Rc<NamespaceTreeNode> {
    ty::with_path(cx.tcx(), def_id, |path| {
        // prepend crate name if not already present
        let krate = if def_id.krate == ast::LOCAL_CRATE {
            let crate_namespace_ident = token::str_to_ident(crate_root_namespace(cx));
            Some(ast_map::PathMod(crate_namespace_ident.name))
        } else {
            None
        };
        let mut path = krate.move_iter().chain(path).peekable();

        let mut current_key = Vec::new();
        let mut parent_node: Option<Rc<NamespaceTreeNode>> = None;

        // Create/Lookup namespace for each element of the path.
        loop {
            // Emulate a for loop so we can use peek below.
            let path_element = match path.next() {
                Some(e) => e,
                None => break
            };
            // Ignore the name of the item (the last path element).
            if path.peek().is_none() {
                break;
            }

            let name = path_element.name();
            current_key.push(name);

            let existing_node = debug_context(cx).namespace_map.borrow()
                                                 .find_copy(&current_key);
            let current_node = match existing_node {
                Some(existing_node) => existing_node,
                None => {
                    // create and insert
                    let parent_scope = match parent_node {
                        Some(ref node) => node.scope,
                        None => ptr::mut_null()
                    };
                    let namespace_name = token::get_name(name);
                    let scope = namespace_name.get().with_c_str(|namespace_name| {
                        unsafe {
                            llvm::LLVMDIBuilderCreateNameSpace(
                                DIB(cx),
                                parent_scope,
                                namespace_name,
                                // cannot reconstruct file ...
                                ptr::mut_null(),
                                // ... or line information, but that's not so important.
                                0)
                        }
                    });

                    let node = Rc::new(NamespaceTreeNode {
                        name: name,
                        scope: scope,
                        parent: parent_node.map(|parent| parent.downgrade()),
                    });

                    debug_context(cx).namespace_map.borrow_mut()
                                     .insert(current_key.clone(), node.clone());

                    node
                }
            };

            parent_node = Some(current_node);
        }

        match parent_node {
            Some(node) => node,
            None => {
                cx.sess().bug(format!("debuginfo::namespace_for_item(): \
                                       path too short for {:?}",
                                      def_id).as_slice());
            }
        }
    })
}
