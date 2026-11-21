//! Collection of running Wasm instances.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::{alloc::Allocator, fmt, marker::PhantomData};

use crate::low_level_vm;

/// List of running Wasm instances, indexed by a 32 bytes identifier.
pub struct Executor<
    TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> {
    /// Running instances.
    instances: imbl::GenericHashMap<
        [u8; 32],
        InstanceRef<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>,
        fnv::FnvBuildHasher,
        imbl::shared_ptr::DefaultSharedPtr,
    >,
    /// Allocator of the native stacks of the instances.
    stack_allocator: TStackAlloc,
    /// Allocator of the linear memories of the instances.
    memory_allocator: TMemAlloc,
    /// Proof that the trap handlers are installed, necessary in order to create instances.
    trap_handlers: low_level_vm::TrapHandlers,
}

/// Shared reference to a running instance.
pub struct InstanceRef<
    TModule,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> {
    inner: Arc<InstanceRefInner>,
}

impl<
    TModule,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> Clone for InstanceRef<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>
{
    #[inline]
    fn clone(&self) -> Self {
        InstanceRef {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct InstanceRefInner {
    /// If `Some`, the instance is ready to resume execution.
    ready: atomicbox::AtomicOptionBox<
        low_level_vm::Instance<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>,
    >,
    /// List of other instances that ready to deliver a request or notification to
    /// the instance, and that are thus parked.
    waiting_to_deliver: crossbeam_queue::SegQueue<
        Box<low_level_vm::Instance<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>>,
    >,
}

/// Prototype for an [`Executor`].
pub struct ExecutorBuilder<TStackAlloc: Allocator, TMemAlloc: Allocator> {
    stack_allocator: TStackAlloc,
    memory_allocator: TMemAlloc,
    trap_handlers: low_level_vm::TrapHandlers,
}

impl<TStackAlloc: Allocator, TMemAlloc: Allocator> ExecutorBuilder<TStackAlloc, TMemAlloc> {
    /// Initializes a new builder.
    ///
    /// The native stacks of the instances are allocated with `stack_allocator`, and their linear
    /// memories with `memory_allocator`. See [`low_level_vm::Instance::new`] for the constraints
    /// on these allocators.
    #[inline]
    pub fn new(
        stack_allocator: TStackAlloc,
        memory_allocator: TMemAlloc,
        trap_handlers: low_level_vm::TrapHandlers,
    ) -> Self {
        ExecutorBuilder {
            stack_allocator,
            memory_allocator,
            trap_handlers,
        }
    }

    /// Builds the [`Executor`]. It initially doesn't contain any instance.
    #[inline]
    pub fn build<
        TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
        TCodeAlloc: Allocator,
        TCtxt,
    >(
        self,
    ) -> Executor<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc> {
        Executor {
            instances: imbl::GenericHashMap::with_hasher(fnv::FnvBuildHasher::default()),
            stack_allocator: self.stack_allocator,
            memory_allocator: self.memory_allocator,
            trap_handlers: self.trap_handlers,
        }
    }
}

/// Prototype for a [`Module`].
///
/// The handlers of the interfaces set with [`ModuleBuilder::set_interface_as_native`] receive a
/// mutable reference to a `TCtxt`.
pub struct ModuleBuilder<'a, TCtxt> {
    /// Wasm bytecode of the module. Not validated yet.
    wasm: &'a [u8],
    /// Imported functions of the module, ordered by import index.
    imports: Vec<ModuleImport>,
    /// Native handler of each imported function, or `None` if the import parks the execution.
    /// Ordered the same way as `imports`.
    native_handlers: Vec<Option<fn(&mut TCtxt, MessageAccess<'_>) -> Outcome>>,
}

/// Access to a message that the Wasm code sends on an interface handled natively. See
/// [`ModuleBuilder::set_interface_as_native`].
#[derive(Clone, Copy)]
pub struct MessageAccess<'a> {
    /// Parameters passed by the Wasm code to the imported function.
    inner: low_level_vm::ParamsAccess<'a>,
}

impl MessageAccess<'_> {
    /// Returns the parameter that the Wasm code has passed to the imported function.
    pub fn raw_param(&self) -> low_level_vm::Value {
        // `low_level_vm::Builder::build` checks that all imports have exactly one parameter.
        self.inner.get(0).unwrap_or_else(|| unreachable!())
    }
}

impl fmt::Debug for MessageAccess<'_> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

/// Value returned by the handlers registered with [`ModuleBuilder::set_interface_as_native`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Continue the execution of the Wasm code immediately, with this value as return value of
    /// the imported function.
    Resume(low_level_vm::Value),
    /// Suspend the execution of the Wasm code, as if the interface wasn't handled natively.
    Park,
}

/// Function imported by a [`Module`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleImport {
    /// Function named `request` in the module `interface_<hex>`, where `<hex>` is the given
    /// interface.
    Request {
        /// Interface the request is sent on.
        interface: [u8; 32],
    },
    /// Function named `notification` in the module `interface_<hex>`, where `<hex>` is the
    /// given interface.
    Notification {
        /// Interface the notification is sent on.
        interface: [u8; 32],
    },
}

/// Error that can happen in [`ModuleBuilder::new`].
#[derive(Debug)]
pub enum ModuleBuilderError {
    /// The Wasm bytecode is malformed.
    Parse(wasmparser::BinaryReaderError),
    /// An imported function isn't in a module named `interface_<hex>`, where `<hex>` is the
    /// lowercase hexadecimal representation of 32 bytes, or its name is neither `request` nor
    /// `notification`.
    InvalidImportName {
        /// Module name of the offending imported function.
        module: String,
        /// Name of the offending imported function.
        name: String,
    },
}

impl From<wasmparser::BinaryReaderError> for ModuleBuilderError {
    #[inline]
    fn from(err: wasmparser::BinaryReaderError) -> Self {
        ModuleBuilderError::Parse(err)
    }
}

impl fmt::Display for ModuleBuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleBuilderError::Parse(err) => write!(f, "invalid Wasm: {err}"),
            ModuleBuilderError::InvalidImportName { module, name } => {
                write!(f, "invalid imported function: {module:?}.{name:?}")
            }
        }
    }
}

impl<'a, TCtxt> ModuleBuilder<'a, TCtxt> {
    /// Parses the imports of the given Wasm module.
    ///
    /// Each imported function must be in a module named `interface_<hex>`, where `<hex>` is the
    /// lowercase hexadecimal representation of the 32 bytes identifier of the interface, and
    /// must be named either `request` or `notification`.
    ///
    /// The Wasm bytecode is only fully validated by [`ModuleBuilder::build`].
    pub fn new(wasm: &'a [u8]) -> Result<Self, ModuleBuilderError> {
        let mut imports = Vec::new();

        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            let wasmparser::Payload::ImportSection(reader) = payload? else {
                continue;
            };

            for import in reader.into_imports() {
                let import = import?;
                if !matches!(import.ty, wasmparser::TypeRef::Func(_)) {
                    // Non-function imports are rejected by `ModuleBuilder::build`.
                    continue;
                }

                let is_request = match import.name {
                    "request" => true,
                    "notification" => false,
                    _ => {
                        return Err(ModuleBuilderError::InvalidImportName {
                            module: String::from(import.module),
                            name: String::from(import.name),
                        });
                    }
                };

                let Some(hex) = import
                    .module
                    .strip_prefix("interface_")
                    .filter(|hex| hex.len() == 64)
                else {
                    return Err(ModuleBuilderError::InvalidImportName {
                        module: String::from(import.module),
                        name: String::from(import.name),
                    });
                };

                let mut interface = [0u8; 32];
                for (byte, digits) in interface.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
                    for digit in digits {
                        let nibble = match digit {
                            b'0'..=b'9' => digit - b'0',
                            b'a'..=b'f' => digit - b'a' + 10,
                            _ => {
                                return Err(ModuleBuilderError::InvalidImportName {
                                    module: String::from(import.module),
                                    name: String::from(import.name),
                                });
                            }
                        };
                        *byte = (*byte << 4) | nibble;
                    }
                }

                imports.push(if is_request {
                    ModuleImport::Request { interface }
                } else {
                    ModuleImport::Notification { interface }
                });
            }
        }

        let native_handlers = alloc::vec![None; imports.len()];
        Ok(ModuleBuilder {
            wasm,
            imports,
            native_handlers,
        })
    }

    /// Makes the `request` and `notification` imported functions of the given interface call
    /// `handler` directly, instead of parking the execution. Overwrites any previously-set
    /// handler for this interface.
    ///
    /// Has no effect if the module doesn't use this interface.
    ///
    /// `handler` is called directly by the compiled Wasm code. See
    /// [`low_level_vm::Builder::register_import`] for the constraints that apply to it.
    pub fn set_interface_as_native(
        &mut self,
        interface: [u8; 32],
        handler: fn(&mut TCtxt, MessageAccess<'_>) -> Outcome,
    ) {
        for (import, native_handler) in self.imports.iter().zip(&mut self.native_handlers) {
            match import {
                ModuleImport::Request { interface: i }
                | ModuleImport::Notification { interface: i }
                    if *i == interface =>
                {
                    *native_handler = Some(handler);
                }
                _ => {}
            }
        }
    }

    /// Validates and compiles the Wasm module.
    ///
    /// The compiled code is written to memory allocated with `code_allocator`, which must be
    /// executable.
    pub fn build<TCodeAlloc: Allocator>(
        self,
        code_allocator: TCodeAlloc,
    ) -> Result<Module<TCodeAlloc, TCtxt>, low_level_vm::Error>
    where
        TCtxt: 'static,
    {
        let mut builder = low_level_vm::Builder::new(self.wasm, code_allocator);
        for native_handler in self.native_handlers {
            match native_handler {
                Some(handler) => builder.register_import(move |ctxt, params| {
                    match handler(ctxt, MessageAccess { inner: params }) {
                        Outcome::Resume(value) => low_level_vm::Outcome::Resume(value),
                        Outcome::Park => low_level_vm::Outcome::Park,
                    }
                }),
                None => builder.register_import(|_, _| low_level_vm::Outcome::Park),
            }
        }

        let mut interfaces =
            self.imports
                .iter()
                .map(|import| match import {
                    ModuleImport::Request { interface }
                    | ModuleImport::Notification { interface } => *interface,
                })
                .collect::<Vec<_>>();
        interfaces.sort_unstable();
        interfaces.dedup(); // TODO: no, error

        Ok(Module {
            inner: builder.build()?,
            imports: self.imports,
            interfaces,
        })
    }
}

/// A compiled Wasm module whose imported functions are all interface requests or
/// notifications. See [`ModuleBuilder`].
pub struct Module<TCodeAlloc: Allocator, TCtxt> {
    /// The compiled module.
    inner: low_level_vm::Module<TCodeAlloc, TCtxt>,
    /// Imported functions of the module, ordered by import index.
    // TODO: remove?
    imports: Vec<ModuleImport>,
    /// Interfaces used by the imported functions, sorted and without duplicates.
    interfaces: Vec<[u8; 32]>,
}

/// Prototype for an instance of a [`Module`].
pub struct InstanceBuilder<
    TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> {
    /// Module to instantiate.
    module: TModule,
    /// Private key of the instance.
    private_key: [u8; 32],
    /// Program handling each interface used by the module, or `None` if not set yet. Ordered
    /// the same way as [`Module::interfaces`].
    interface_handlers:
        Vec<Option<InstanceRef<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>>>,
    /// Marker for the `TCodeAlloc` and `TCtxt` generic parameters.
    marker: PhantomData<fn() -> (TCodeAlloc, TCtxt)>,
}

impl<
    TModule: AsRef<Module<TCodeAlloc, TCtxt>>,
    TCodeAlloc: Allocator,
    TCtxt,
    TStackAlloc: Allocator,
    TMemAlloc: Allocator,
> InstanceBuilder<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>
{
    /// Initializes a new builder for an instance of the given module.
    ///
    /// `private_key` is the private key of the instance.
    ///
    /// Initially, no interface has a handler program.
    pub fn new(module: TModule, private_key: &[u8; 32]) -> Self {
        let num_interfaces = AsRef::<Module<TCodeAlloc, TCtxt>>::as_ref(&module)
            .interfaces
            .len();
        InstanceBuilder {
            module,
            private_key: *private_key,
            interface_handlers: (0..num_interfaces).map(|_| None).collect::<Vec<_>>(),
            marker: PhantomData,
        }
    }

    /// Sets the program that handles the messages that the instance sends on the given
    /// interface. Overwrites any previously-set handler.
    ///
    /// # Panic
    ///
    /// Panics if `interface` isn't one of the interfaces used by the module, as returned by
    /// [`Module::interfaces_sorted`].
    pub fn set_interface_handler_program(
        &mut self,
        interface: &[u8; 32],
        handler_program: &InstanceRef<TModule, TCodeAlloc, TCtxt, TStackAlloc, TMemAlloc>,
    ) {
        let Ok(index) = AsRef::<Module<TCodeAlloc, TCtxt>>::as_ref(&self.module)
            .interfaces
            .binary_search(interface)
        else {
            panic!("interface not used by the module")
        };
        self.interface_handlers[index] = Some(handler_program.clone());
    }
}
