//! Collection of running Wasm instances.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{alloc::Allocator, fmt, marker::PhantomData};

use crate::low_level_vm;

/// List of running Wasm instances, indexed by a 32 bytes identifier.
pub struct Executor<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator> {
    /// Running instances.
    instances: imbl::GenericHashMap<
        [u8; 32],
        // TODO: `()` is a placeholder for the context of the instances
        low_level_vm::Instance<
            Arc<low_level_vm::Module<TCodeAlloc, ()>>,
            TCodeAlloc,
            (),
            TStackAlloc,
            TMemAlloc,
        >,
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

/// Prototype for an [`Executor`].
pub struct Builder<TStackAlloc: Allocator, TMemAlloc: Allocator> {
    stack_allocator: TStackAlloc,
    memory_allocator: TMemAlloc,
    trap_handlers: low_level_vm::TrapHandlers,
}

impl<TStackAlloc: Allocator, TMemAlloc: Allocator> Builder<TStackAlloc, TMemAlloc> {
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
        Builder {
            stack_allocator,
            memory_allocator,
            trap_handlers,
        }
    }

    /// Builds the [`Executor`]. It initially doesn't contain any instance.
    #[inline]
    pub fn build<TCodeAlloc: Allocator>(self) -> Executor<TCodeAlloc, TStackAlloc, TMemAlloc> {
        Executor {
            instances: imbl::GenericHashMap::with_hasher(fnv::FnvBuildHasher::default()),
            stack_allocator: self.stack_allocator,
            memory_allocator: self.memory_allocator,
            trap_handlers: self.trap_handlers,
        }
    }
}

/// List of compiled Wasm modules, indexed by a 32 bytes identifier.
pub struct ModulesRegistry<TCodeAlloc: Allocator> {
    // TODO: `()` is a placeholder for the context of the instances
    modules: hashbrown::HashMap<
        [u8; 32],
        Arc<low_level_vm::Module<TCodeAlloc, ()>>,
        fnv::FnvBuildHasher,
    >,
}

/// Prototype for a [`Module`].
pub struct ModuleBuilder<'a> {
    /// Wasm bytecode of the module. Not validated yet.
    wasm: &'a [u8],
    /// Imported functions of the module, ordered by import index.
    imports: Vec<ModuleImport>,
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

impl<'a> ModuleBuilder<'a> {
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

        Ok(ModuleBuilder { wasm, imports })
    }

    /// Returns the list of imported functions, ordered by import index.
    #[inline]
    pub fn imports_by_index(&self) -> impl ExactSizeIterator<Item = ModuleImport> {
        self.imports.iter().copied()
    }

    /// Validates and compiles the Wasm module.
    ///
    /// The compiled code is written to memory allocated with `code_allocator`, which must be
    /// executable.
    pub fn build<TCodeAlloc: Allocator>(
        self,
        code_allocator: TCodeAlloc,
    ) -> Result<Module<TCodeAlloc>, low_level_vm::Error> {
        let mut builder = low_level_vm::Builder::new(self.wasm, code_allocator);
        for _ in &self.imports {
            builder.register_import(|_, _| low_level_vm::Outcome::Park);
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
        interfaces.dedup();

        Ok(Module {
            inner: builder.build()?,
            imports: self.imports,
            interfaces,
        })
    }
}

/// A compiled Wasm module whose imported functions are all interface requests or
/// notifications. See [`ModuleBuilder`].
pub struct Module<TCodeAlloc: Allocator> {
    // TODO: `()` is a placeholder for the context of the instances
    /// The compiled module. All its imports park the execution.
    inner: low_level_vm::Module<TCodeAlloc, ()>,
    /// Imported functions of the module, ordered by import index.
    imports: Vec<ModuleImport>,
    /// Interfaces used by the imported functions, sorted and without duplicates.
    interfaces: Vec<[u8; 32]>,
}

impl<TCodeAlloc: Allocator> Module<TCodeAlloc> {
    /// Returns the list of imported functions, ordered by import index.
    ///
    /// The index of an import is the one found in [`low_level_vm::RunOutcome::ImportCall`].
    #[inline]
    pub fn imports_by_index(&self) -> impl ExactSizeIterator<Item = ModuleImport> {
        self.imports.iter().copied()
    }

    /// Returns the list of interfaces that this module uses, sorted by identifier and without
    /// duplicates.
    #[inline]
    pub fn interfaces_sorted(&self) -> impl ExactSizeIterator<Item = [u8; 32]> {
        self.interfaces.iter().copied()
    }
}

/// Prototype for an instance of a [`Module`].
pub struct InstanceBuilder<TModule, TCodeAlloc: Allocator> {
    /// Module to instantiate.
    module: TModule,
    /// Private key of the instance.
    private_key: [u8; 32],
    /// Public key of the program handling each interface used by the module, or `None` if not
    /// set yet. Ordered the same way as [`Module::interfaces`].
    interface_handlers: Vec<Option<[u8; 32]>>,
    /// Marker for the `TCodeAlloc` generic parameter.
    marker: PhantomData<fn() -> TCodeAlloc>,
}

impl<TModule: AsRef<Module<TCodeAlloc>>, TCodeAlloc: Allocator>
    InstanceBuilder<TModule, TCodeAlloc>
{
    /// Initializes a new builder for an instance of the given module.
    ///
    /// `private_key` is the private key of the instance.
    ///
    /// Initially, no interface has a handler program.
    pub fn new(module: TModule, private_key: &[u8; 32]) -> Self {
        let num_interfaces = module.as_ref().interfaces.len();
        InstanceBuilder {
            module,
            private_key: *private_key,
            interface_handlers: alloc::vec![None; num_interfaces],
            marker: PhantomData,
        }
    }

    /// Sets the program, identified by its public key, that handles the messages that the
    /// instance sends on the given interface. Overwrites any previously-set handler.
    ///
    /// # Panic
    ///
    /// Panics if `interface` isn't one of the interfaces used by the module, as returned by
    /// [`Module::interfaces_sorted`].
    pub fn set_interface_handler_program(
        &mut self,
        interface: &[u8; 32],
        handler_public_key: &[u8; 32],
    ) {
        let Ok(index) = self.module.as_ref().interfaces.binary_search(interface) else {
            panic!("interface not used by the module")
        };
        self.interface_handlers[index] = Some(*handler_public_key);
    }
}
