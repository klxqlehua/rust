#![feature(rustc_private, macro_at_most_once_rep)]
#![allow(intra_doc_link_resolution_failure)]

extern crate syntax;
#[macro_use]
extern crate rustc;
extern crate rustc_codegen_utils;
extern crate rustc_incremental;
extern crate rustc_mir;
extern crate rustc_target;
#[macro_use]
extern crate rustc_data_structures;

extern crate ar;
#[macro_use]
extern crate bitflags;
extern crate faerie;
//extern crate goblin;
extern crate cranelift;
extern crate cranelift_faerie;
extern crate cranelift_module;
extern crate cranelift_simplejit;
extern crate target_lexicon;

use std::any::Any;
use std::fs::File;
use std::path::Path;
use std::sync::{mpsc, Arc};

use rustc::dep_graph::DepGraph;
use rustc::middle::cstore::MetadataLoader;
use rustc::session::{
    config::{CrateType, OutputFilenames},
    CompileIncomplete,
};
use rustc::ty::query::Providers;
use rustc_codegen_utils::codegen_backend::CodegenBackend;
use rustc_codegen_utils::link::{build_link_meta, out_filename};
use rustc_data_structures::owning_ref::{self, OwningRef};
use syntax::symbol::Symbol;

use cranelift::codegen::settings;
use cranelift_faerie::*;

struct NonFatal(pub String);

macro_rules! unimpl {
    ($($tt:tt)*) => {
        panic!(::NonFatal(format!($($tt)*)));
    };
}

mod abi;
mod analyze;
mod base;
mod common;
mod constant;
mod pretty_clif;

mod prelude {
    pub use std::any::Any;
    pub use std::collections::{HashMap, HashSet};

    pub use rustc::hir::def_id::{DefId, LOCAL_CRATE};
    pub use rustc::mir;
    pub use rustc::mir::interpret::AllocId;
    pub use rustc::mir::*;
    pub use rustc::session::Session;
    pub use rustc::ty::layout::{self, LayoutOf, Size, TyLayout};
    pub use rustc::ty::{
        self, subst::Substs, FnSig, Instance, InstanceDef, ParamEnv, PolyFnSig, Ty, TyCtxt,
        TypeAndMut, TypeFoldable, TypeVariants,
    };
    pub use rustc_data_structures::{indexed_vec::Idx, sync::Lrc};
    pub use rustc_mir::monomorphize::{collector, MonoItem};
    pub use syntax::ast::{FloatTy, IntTy, UintTy};
    pub use syntax::codemap::DUMMY_SP;

    pub use cranelift::codegen::ir::{
        condcodes::IntCC, function::Function, ExternalName, FuncRef, Inst, StackSlot,
    };
    pub use cranelift::codegen::Context;
    pub use cranelift::prelude::*;
    pub use cranelift_module::{
        Backend, DataContext, DataId, FuncId, Linkage, Module, Writability,
    };
    pub use cranelift_simplejit::{SimpleJITBackend, SimpleJITBuilder};

    pub use crate::abi::*;
    pub use crate::base::{trans_operand, trans_place};
    pub use crate::common::Variable;
    pub use crate::common::*;

    pub use crate::CodegenCx;
}

use crate::prelude::*;

pub struct CodegenCx<'a, 'tcx: 'a, B: Backend + 'a> {
    pub tcx: TyCtxt<'a, 'tcx, 'tcx>,
    pub module: &'a mut Module<B>,
    pub constants: HashMap<AllocId, DataId>,
}

struct CraneliftMetadataLoader;

impl MetadataLoader for CraneliftMetadataLoader {
    fn get_rlib_metadata(
        &self,
        _target: &rustc_target::spec::Target,
        path: &Path,
    ) -> Result<owning_ref::ErasedBoxRef<[u8]>, String> {
        let mut archive = ar::Archive::new(File::open(path).map_err(|e| format!("{:?}", e))?);
        // Iterate over all entries in the archive:
        while let Some(entry_result) = archive.next_entry() {
            let mut entry = entry_result.map_err(|e| format!("{:?}", e))?;
            if entry.header().identifier() == b".rustc.clif_metadata" {
                let mut buf = Vec::new();
                ::std::io::copy(&mut entry, &mut buf).map_err(|e| format!("{:?}", e))?;
                let buf: OwningRef<Vec<u8>, [u8]> = OwningRef::new(buf).into();
                return Ok(rustc_erase_owner!(buf.map_owner_box()));
            }
        }

        Err("couldn't find metadata entry".to_string())
        //self.get_dylib_metadata(target, path)
    }

    fn get_dylib_metadata(
        &self,
        _target: &rustc_target::spec::Target,
        _path: &Path,
    ) -> Result<owning_ref::ErasedBoxRef<[u8]>, String> {
        //use goblin::Object;

        //let buffer = ::std::fs::read(path).map_err(|e|format!("{:?}", e))?;
        /*match Object::parse(&buffer).map_err(|e|format!("{:?}", e))? {
            Object::Elf(elf) => {
                println!("elf: {:#?}", &elf);
            },
            Object::PE(pe) => {
                println!("pe: {:#?}", &pe);
            },
            Object::Mach(mach) => {
                println!("mach: {:#?}", &mach);
            },
            Object::Archive(archive) => {
                return Err(format!("archive: {:#?}", &archive));
            },
            Object::Unknown(magic) => {
                return Err(format!("unknown magic: {:#x}", magic))
            }
        }*/
        Err("dylib metadata loading is not yet supported".to_string())
    }
}

struct CraneliftCodegenBackend;

struct OngoingCodegen {
    product: cranelift_faerie::FaerieProduct,
    metadata: Vec<u8>,
    crate_name: Symbol,
}

impl CodegenBackend for CraneliftCodegenBackend {
    fn init(&self, sess: &Session) {
        for cty in sess.opts.crate_types.iter() {
            match *cty {
                CrateType::Rlib | CrateType::Dylib | CrateType::Executable => {}
                _ => {
                    sess.parse_sess.span_diagnostic.warn(&format!(
                        "LLVM unsupported, so output type {} is not supported",
                        cty
                    ));
                }
            }
        }
    }

    fn metadata_loader(&self) -> Box<MetadataLoader + Sync> {
        Box::new(CraneliftMetadataLoader)
    }

    fn provide(&self, providers: &mut Providers) {
        rustc_codegen_utils::symbol_names::provide(providers);

        providers.target_features_whitelist = |_tcx, _cnum| Lrc::new(Default::default());
        providers.is_reachable_non_generic = |_tcx, _defid| true;
        providers.exported_symbols = |_tcx, _crate| Arc::new(Vec::new());
    }
    fn provide_extern(&self, providers: &mut Providers) {
        providers.is_reachable_non_generic = |_tcx, _defid| true;
    }

    fn codegen_crate<'a, 'tcx>(
        &self,
        tcx: TyCtxt<'a, 'tcx, 'tcx>,
        _rx: mpsc::Receiver<Box<Any + Send>>,
    ) -> Box<Any> {
        use rustc_mir::monomorphize::item::MonoItem;

        rustc_codegen_utils::check_for_rustc_errors_attr(tcx);
        rustc_codegen_utils::symbol_names_test::report_symbol_names(tcx);
        rustc_incremental::assert_dep_graph(tcx);
        rustc_incremental::assert_module_sources::assert_module_sources(tcx);
        rustc_mir::monomorphize::assert_symbols_are_distinct(
            tcx,
            collector::collect_crate_mono_items(tcx, collector::MonoItemCollectionMode::Eager)
                .0
                .iter(),
        );
        //::rustc::middle::dependency_format::calculate(tcx);
        let _ = tcx.link_args(LOCAL_CRATE);
        let _ = tcx.native_libraries(LOCAL_CRATE);
        for mono_item in
            collector::collect_crate_mono_items(tcx, collector::MonoItemCollectionMode::Eager).0
        {
            match mono_item {
                MonoItem::Fn(inst) => {
                    let def_id = inst.def_id();
                    if def_id.is_local() {
                        let _ = inst.def.is_inline(tcx);
                        let _ = tcx.codegen_fn_attrs(def_id);
                    }
                }
                _ => {}
            }
        }
        tcx.sess.abort_if_errors();

        let link_meta = ::build_link_meta(tcx.crate_hash(LOCAL_CRATE));
        let metadata = tcx.encode_metadata(&link_meta);

        let mut flags_builder = settings::builder();
        flags_builder.enable("is_pic").unwrap();
        let flags = settings::Flags::new(flags_builder);
        let isa = cranelift::codegen::isa::lookup(target_lexicon::Triple::host())
            .unwrap()
            .finish(flags);
        let mut module: Module<SimpleJITBackend> = Module::new(SimpleJITBuilder::new());
        let mut context = Context::new();

        {
            use std::io::Write;
            let mut cx = CodegenCx {
                tcx,
                module: &mut module,
                constants: HashMap::new(),
            };

            let mut log = ::std::fs::File::create("../target/log.txt").unwrap();

            for mono_item in
                collector::collect_crate_mono_items(tcx, collector::MonoItemCollectionMode::Eager).0
            {
                let cx = &mut cx;
                let context = &mut context;
                let res = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(move || {
                    base::trans_mono_item(cx, context, mono_item);
                }));
                if let Err(err) = res {
                    match err.downcast::<NonFatal>() {
                        Ok(non_fatal) => {
                            writeln!(log, "{}", &non_fatal.0);
                            tcx.sess.err(&non_fatal.0)
                        }
                        Err(err) => ::std::panic::resume_unwind(err),
                    }
                }
            }
        }

        tcx.sess.warn("Compiled everything");

        // TODO: this doesn't work most of the time
        if false {
            module.finalize_all();
            tcx.sess.warn("Finalized everything");

            for mono_item in
                collector::collect_crate_mono_items(tcx, collector::MonoItemCollectionMode::Eager).0
            {
                let inst = match mono_item {
                    MonoItem::Fn(inst) => inst,
                    _ => continue,
                };

                //if tcx.absolute_item_path_str(inst.def_id()) != "example::ret_42" {
                if tcx.absolute_item_path_str(inst.def_id()) != "example::option_unwrap_or" {
                    continue;
                }

                let fn_ty = inst.ty(tcx);
                let sig = cton_sig_from_fn_ty(tcx, fn_ty);
                let def_path_based_names =
                    ::rustc_mir::monomorphize::item::DefPathBasedNames::new(tcx, false, false);
                let mut name = String::new();
                def_path_based_names.push_instance_as_string(inst, &mut name);
                let func_id = module
                    .declare_function(&name, Linkage::Import, &sig)
                    .unwrap();

                let finalized_function: *const u8 = module.finalize_function(func_id);
                /*let f: extern "C" fn(&mut u32) = unsafe { ::std::mem::transmute(finalized_function) };
                let mut res = 0u32;
                f(&mut res);
                tcx.sess.warn(&format!("ret_42 returned {}", res));*/
                let f: extern "C" fn(&mut bool, &u8, bool) =
                    unsafe { ::std::mem::transmute(finalized_function) };
                let mut res = false;
                f(&mut res, &3, false);
                tcx.sess.warn(&format!("option_unwrap_or returned {}", res));
            }

            module.finish();
        }

        let mut translated_module: Module<FaerieBackend> = Module::new(
            FaerieBuilder::new(
                isa,
                "some_file.o".to_string(),
                FaerieTrapCollection::Disabled,
                FaerieBuilder::default_libcall_names(),
            ).unwrap(),
        );

        Box::new(OngoingCodegen {
            product: translated_module.finish(),
            metadata: metadata.raw_data,
            crate_name: tcx.crate_name(LOCAL_CRATE),
        })
    }

    fn join_codegen_and_link(
        &self,
        ongoing_codegen: Box<Any>,
        sess: &Session,
        _dep_graph: &DepGraph,
        outputs: &OutputFilenames,
    ) -> Result<(), CompileIncomplete> {
        let ongoing_codegen = *ongoing_codegen
            .downcast::<OngoingCodegen>()
            .expect("Expected CraneliftCodegenBackend's OngoingCodegen, found Box<Any>");

        let mut artifact = ongoing_codegen.product.artifact;
        let metadata = ongoing_codegen.metadata;

        artifact
            .declare_with(
                ".rustc.clif_metadata",
                faerie::artifact::Decl::Data {
                    global: true,
                    writeable: false,
                },
                metadata.clone(),
            ).unwrap();

        for &crate_type in sess.opts.crate_types.iter() {
            if crate_type != CrateType::Rlib
            /*&& crate_type != CrateType::Dylib*/
            {
                sess.fatal(&format!("Unsupported crate type: {:?}", crate_type));
            }
            let output_name = out_filename(
                sess,
                crate_type,
                &outputs,
                &ongoing_codegen.crate_name.as_str(),
            );
            let file = File::create(&output_name).unwrap();
            let mut builder = ar::Builder::new(file);
            builder
                .append(
                    &ar::Header::new(b".rustc.clif_metadata".to_vec(), metadata.len() as u64),
                    ::std::io::Cursor::new(metadata.clone()),
                ).unwrap();
            //artifact.write(file).unwrap();
        }

        sess.abort_if_errors();
        if !sess.opts.crate_types.contains(&CrateType::Rlib)
            && !sess.opts.crate_types.contains(&CrateType::Dylib)
        {
            sess.fatal("Executables are not supported by the metadata-only backend.");
        }
        Ok(())
    }
}

/// This is the entrypoint for a hot plugged rustc_codegen_cranelift
#[no_mangle]
pub fn __rustc_codegen_backend() -> Box<CodegenBackend> {
    Box::new(CraneliftCodegenBackend)
}
