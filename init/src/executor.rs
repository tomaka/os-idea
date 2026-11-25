//! Collection of Wasm instances and scheduling of their execution.
//!
//! The [`Executor`] holds a queue of instances that are ready to execute. Instances are added
//! to it with [`InstanceBuilder::build`], and [`Executor::run`] pops an instance from the queue
//! and runs it until it parks itself.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{alloc::Allocator, fmt};

use crate::low_level_vm;

/// Queue of Wasm instances that are ready to execute.
pub struct Executor<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator> {
    /// Instances that are ready to unpark, alongside with what to do when unparking them.
    ready: crossbeam_queue::SegQueue<ReadyInstance<TCodeAlloc, TStackAlloc, TMemAlloc>>,
    /// Proof that the trap handlers are installed, necessary in order to create instances.
    trap_handlers: low_level_vm::TrapHandlers,
    /// Interfaces implemented natively by the host. See
    /// [`ExecutorBuilder::register_native_interface`].
    native_interfaces: hashbrown::HashSet<[u8; 32], fnv::FnvBuildHasher>,
    /// Delivery queue of each instance, indexed by public key of the instance.
    // TODO: entries are never removed, even when the instance is destroyed
    // TODO: do better than a spin::Mutex
    delivery_queues: spin::Mutex<
        hashbrown::HashMap<
            [u8; 32],
            Arc<crossbeam_queue::SegQueue<PendingDelivery<TCodeAlloc, TStackAlloc, TMemAlloc>>>,
            fnv::FnvBuildHasher,
        >,
    >,
}

/// Prototype for an [`Executor`].
pub struct ExecutorBuilder {
    /// See [`Executor::trap_handlers`].
    trap_handlers: low_level_vm::TrapHandlers,
    /// See [`Executor::native_interfaces`].
    native_interfaces: hashbrown::HashSet<[u8; 32], fnv::FnvBuildHasher>,
}

impl ExecutorBuilder {
    /// Initializes a new builder. Initially, no interface is implemented natively.
    #[inline]
    pub fn new(trap_handlers: low_level_vm::TrapHandlers) -> Self {
        ExecutorBuilder {
            trap_handlers,
            native_interfaces: hashbrown::HashSet::with_hasher(fnv::FnvBuildHasher::default()),
        }
    }

    /// Registers the given interface as being implemented natively by the host, for all the
    /// instances of the executor. Has no effect if the interface is already registered.
    #[inline]
    pub fn register_native_interface(&mut self, interface: [u8; 32]) {
        self.native_interfaces.insert(interface);
    }

    /// Builds the [`Executor`]. It initially doesn't contain any instance.
    #[inline]
    pub fn build<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator>(
        self,
    ) -> Executor<TCodeAlloc, TStackAlloc, TMemAlloc> {
        Executor {
            ready: crossbeam_queue::SegQueue::new(),
            trap_handlers: self.trap_handlers,
            native_interfaces: self.native_interfaces,
            delivery_queues: spin::Mutex::new(hashbrown::HashMap::with_hasher(
                fnv::FnvBuildHasher::default(),
            )),
        }
    }
}

/// Entry in [`Executor::ready`].
struct ReadyInstance<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator> {
    /// The instance ready to unpark.
    instance: Instance<TCodeAlloc, TStackAlloc, TMemAlloc>,
    /// How to unpark the instance.
    status: ReadyStatus,
}

/// How to unpark an instance in [`Executor::ready`].
enum ReadyStatus {
    /// The instance has never been executed. Its `main` function must be called.
    NotStarted,
    /// The instance is suspended in a call to an imported function of type
    /// [`ModuleImport::Request`] or [`ModuleImport::Notification`], and the message has been
    /// delivered. The call must return.
    MessageDelivered,
    /// The instance is suspended in a call to an imported function of type
    /// [`ModuleImport::Request`] on an interface implemented natively, and the request must be
    /// sent.
    SendingNativeInterfaceRequest {
        /// Interface the request is sent on.
        // TODO: not used yet, as native interfaces aren't implemented
        #[expect(dead_code)]
        interface: [u8; 32],
        /// Offset in the linear memory of the instance of the buffer containing the request.
        // TODO: not used yet, as native interfaces aren't implemented
        #[expect(dead_code)]
        buffer_ptr: u32,
        /// Size in bytes of the buffer containing the request. The range
        /// `buffer_ptr..buffer_ptr + buffer_len` is always within the bounds of the linear
        /// memory, as the linear memory can only grow.
        // TODO: not used yet, as native interfaces aren't implemented
        #[expect(dead_code)]
        buffer_len: u32,
    },
    /// The instance is suspended in a call to an imported function of type
    /// [`ModuleImport::Notification`] on an interface implemented natively, and the
    /// notification must be sent.
    SendingNativeInterfaceNotification {
        /// Interface the notification is sent on.
        // TODO: not used yet, as native interfaces aren't implemented
        #[expect(dead_code)]
        interface: [u8; 32],
        /// Offset in the linear memory of the instance of the buffer containing the
        /// notification.
        // TODO: not used yet, as native interfaces aren't implemented
        #[expect(dead_code)]
        buffer_ptr: u32,
        /// Size in bytes of the buffer containing the notification. The range
        /// `buffer_ptr..buffer_ptr + buffer_len` is always within the bounds of the linear
        /// memory, as the linear memory can only grow.
        // TODO: not used yet, as native interfaces aren't implemented
        #[expect(dead_code)]
        buffer_len: u32,
    },
    /// The instance is suspended in a call to an imported function of type
    /// [`ModuleImport::WaitForMessage`], and is waiting for a message to be delivered to it.
    WaitingForMessage {
        /// Offset in the linear memory of the instance of the buffer where to write the
        /// message.
        buffer_ptr: u32,
        /// Size in bytes of the buffer where to write the message. The range
        /// `buffer_ptr..buffer_ptr + buffer_len` is always within the bounds of the linear
        /// memory, as the linear memory can only grow.
        buffer_len: u32,
    },
}

/// Entry in [`Instance::pending_delivery`].
struct PendingDelivery<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator> {
    /// Instance that has sent the message. Suspended in a call to an imported function of type
    /// [`ModuleImport::Request`] or [`ModuleImport::Notification`].
    instance: Instance<TCodeAlloc, TStackAlloc, TMemAlloc>,
    /// Offset in the linear memory of [`PendingDelivery::instance`] of the buffer containing
    /// the message.
    // TODO: not used yet, as message delivery isn't implemented
    #[expect(dead_code)]
    buffer_ptr: u32,
    /// Size in bytes of the buffer containing the message. The range
    /// `buffer_ptr..buffer_ptr + buffer_len` is always within the bounds of the linear memory
    /// of [`PendingDelivery::instance`], as the linear memory can only grow.
    // TODO: not used yet, as message delivery isn't implemented
    #[expect(dead_code)]
    buffer_len: u32,
}

/// Instantiated [`Module`].
struct Instance<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator> {
    /// The low-level instance.
    vm: low_level_vm::Instance<
        Arc<low_level_vm::Module<TCodeAlloc>>,
        TCodeAlloc,
        TStackAlloc,
        TMemAlloc,
    >,
    /// Imported functions of the module, ordered by import index. See [`Module::imports`].
    imports: Arc<[ModuleImport]>,
    /// Public key of the instance.
    // TODO: not used yet
    #[expect(dead_code)]
    public_key: [u8; 32],
    /// Delivery queue of the instance handling each interface used by the module, indexed by
    /// interface. Doesn't contain the interfaces implemented natively.
    interface_handlers: hashbrown::HashMap<
        [u8; 32],
        Arc<crossbeam_queue::SegQueue<PendingDelivery<TCodeAlloc, TStackAlloc, TMemAlloc>>>,
        fnv::FnvBuildHasher,
    >,
    /// Messages waiting to be delivered to this instance.
    pending_delivery:
        Arc<crossbeam_queue::SegQueue<PendingDelivery<TCodeAlloc, TStackAlloc, TMemAlloc>>>,
}

impl<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator>
    Executor<TCodeAlloc, TStackAlloc, TMemAlloc>
{
    /// Pops an instance from the queue of instances that are ready, and runs it until it parks
    /// itself, returns from its `main` function, or fails. In the last two cases, the instance
    /// is destroyed.
    ///
    /// Does nothing if no instance is ready.
    // TODO: pause if no instance is ready
    pub fn run(&self) {
        let Some(ReadyInstance {
            mut instance,
            status,
        }) = self.ready.pop()
        else {
            return;
        };

        let mut outcome = match status {
            ReadyStatus::NotStarted => instance.vm.call("main", &[]),
            ReadyStatus::MessageDelivered => instance.vm.resume(None),
            ReadyStatus::WaitingForMessage {
                buffer_ptr,
                buffer_len,
            } => {
                if let Some(delivery) = instance.pending_delivery.pop() {
                    // TODO: deliver the message to `instance`
                    self.ready.push(ReadyInstance {
                        instance: delivery.instance,
                        status: ReadyStatus::MessageDelivered,
                    });
                }
                self.ready.push(ReadyInstance {
                    instance,
                    status: ReadyStatus::WaitingForMessage {
                        buffer_ptr,
                        buffer_len,
                    },
                });
                return;
            }
            // TODO: native interfaces aren't implemented, the instance is currently destroyed
            ReadyStatus::SendingNativeInterfaceRequest { .. }
            | ReadyStatus::SendingNativeInterfaceNotification { .. } => return,
        };

        loop {
            match outcome {
                // TODO: report the error somehow
                Err(_) => return,
                Ok(low_level_vm::RunOutcome::Return(_)) => return,
                Ok(low_level_vm::RunOutcome::ImportCall { import, args }) => {
                    // The low-level VM only reports imports that exist in the module, and
                    // `instance.imports` contains all the imports of the module.
                    let import = usize::try_from(import)
                        .ok()
                        .and_then(|import| instance.imports.get(import))
                        .unwrap_or_else(|| unreachable!());
                    // All the imports have the signature `(i32, i32)`, which has been checked
                    // when building the module.
                    let [
                        low_level_vm::Value::I32(buffer_ptr),
                        low_level_vm::Value::I32(buffer_len),
                    ] = args[..]
                    else {
                        unreachable!()
                    };
                    let buffer_ptr = buffer_ptr.cast_unsigned();
                    let buffer_len = buffer_len.cast_unsigned();
                    // TODO: report the error somehow
                    if usize::try_from(buffer_ptr)
                        .ok()
                        .zip(usize::try_from(buffer_len).ok())
                        .and_then(|(ptr, len)| instance.vm.memory().get(ptr..)?.get(..len))
                        .is_none()
                    {
                        return;
                    }
                    match *import {
                        ModuleImport::Request { interface }
                            if self.native_interfaces.contains(&interface) =>
                        {
                            self.ready.push(ReadyInstance {
                                instance,
                                status: ReadyStatus::SendingNativeInterfaceRequest {
                                    interface,
                                    buffer_ptr,
                                    buffer_len,
                                },
                            });
                        }
                        ModuleImport::Notification { interface }
                            if self.native_interfaces.contains(&interface) =>
                        {
                            self.ready.push(ReadyInstance {
                                instance,
                                status: ReadyStatus::SendingNativeInterfaceNotification {
                                    interface,
                                    buffer_ptr,
                                    buffer_len,
                                },
                            });
                        }
                        ModuleImport::Request { interface }
                        | ModuleImport::Notification { interface } => {
                            // `InstanceBuilder::build` makes sure that all the interfaces used
                            // by the module are either implemented natively or have a handler.
                            let queue = Arc::clone(
                                instance
                                    .interface_handlers
                                    .get(&interface)
                                    .unwrap_or_else(|| unreachable!()),
                            );
                            queue.push(PendingDelivery {
                                instance,
                                buffer_ptr,
                                buffer_len,
                            });
                        }
                        ModuleImport::WaitForMessage => {
                            self.ready.push(ReadyInstance {
                                instance,
                                status: ReadyStatus::WaitingForMessage {
                                    buffer_ptr,
                                    buffer_len,
                                },
                            });
                        }
                    }
                    return;
                }
                Ok(low_level_vm::RunOutcome::MemoryGrow { delta }) => {
                    let result = match instance.vm.grow_memory(delta) {
                        // The number of pages is at most 65536, and thus always fits in an `i32`.
                        Some(old_pages) => {
                            i32::try_from(old_pages).unwrap_or_else(|_| unreachable!())
                        }
                        None => -1,
                    };
                    outcome = instance.vm.resume(Some(low_level_vm::Value::I32(result)));
                }
            }
        }
    }
}

/// Prototype for a [`Module`].
pub struct ModuleBuilder<'a, TCodeAlloc: Allocator> {
    /// The low-level builder.
    inner: low_level_vm::Builder<'a, TCodeAlloc>,
}

impl<'a, TCodeAlloc: Allocator> ModuleBuilder<'a, TCodeAlloc> {
    /// Initializes a new builder for the given Wasm module.
    ///
    /// The compiled code will be written to memory allocated with `code_allocator`. This memory
    /// must be executable.
    #[inline]
    pub fn new(wasm: &'a [u8], code_allocator: TCodeAlloc) -> Self {
        ModuleBuilder {
            inner: low_level_vm::Builder::new(wasm, code_allocator),
        }
    }

    /// Parses, validates and compiles the Wasm module.
    ///
    /// Each imported function of the module must be in a Wasm module named `interface-<hex>`,
    /// where `<hex>` is the lowercase hexadecimal representation of the 32 bytes identifier of
    /// the interface, and must be named either `request` or `notification`, or must be in a Wasm
    /// module named `interface` and be named `wait_for_message`.
    ///
    /// The `wait_for_message` function must have the signature `(i32, i32) -> i32`, and the
    /// `request` and `notification` functions must have the signature `(i32, i32) -> ()`.
    pub fn build(self) -> Result<Module<TCodeAlloc>, ModuleBuilderError> {
        let inner = self.inner.build().map_err(ModuleBuilderError::Vm)?;

        let mut imports = Vec::with_capacity(inner.imports_by_index().len());
        for (module_name, name, signature) in inner.imports_by_index() {
            let interface = module_name
                .strip_prefix("interface-")
                .filter(|hex| hex.len() == 64)
                .and_then(|hex| {
                    let mut interface = [0u8; 32];
                    for (byte, digits) in interface.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
                        for digit in digits {
                            let nibble = match digit {
                                b'0'..=b'9' => digit - b'0',
                                b'a'..=b'f' => digit - b'a' + 10,
                                _ => return None,
                            };
                            *byte = (*byte << 4) | nibble;
                        }
                    }
                    Some(interface)
                });

            let import = match (module_name, interface, name) {
                ("interface", _, "wait_for_message") => ModuleImport::WaitForMessage,
                (_, Some(interface), "request") => ModuleImport::Request { interface },
                (_, Some(interface), "notification") => ModuleImport::Notification { interface },
                _ => {
                    return Err(ModuleBuilderError::InvalidImport {
                        module: String::from(module_name),
                        name: String::from(name),
                    });
                }
            };

            let (expected_params, expected_results): (&[_], &[_]) = match import {
                ModuleImport::WaitForMessage => (
                    &[low_level_vm::ValueType::I32, low_level_vm::ValueType::I32],
                    &[low_level_vm::ValueType::I32],
                ),
                ModuleImport::Request { .. } | ModuleImport::Notification { .. } => (
                    &[low_level_vm::ValueType::I32, low_level_vm::ValueType::I32],
                    &[],
                ),
            };
            if !signature
                .params_ordered()
                .eq(expected_params.iter().copied())
                || !signature
                    .results_ordered()
                    .eq(expected_results.iter().copied())
            {
                return Err(ModuleBuilderError::InvalidImportSignature {
                    module: String::from(module_name),
                    name: String::from(name),
                });
            }

            imports.push(import);
        }

        Ok(Module {
            inner: Arc::new(inner),
            imports: Arc::from(imports),
        })
    }
}

/// Error that can happen in [`ModuleBuilder::build`].
#[derive(Debug)]
pub enum ModuleBuilderError {
    /// Failed to parse, validate or compile the module.
    // TODO: don't expose low_level_vm::Error
    Vm(low_level_vm::Error),
    /// An imported function isn't in a Wasm module named `interface-<hex>`, where `<hex>` is
    /// the lowercase hexadecimal representation of 32 bytes, or its name is neither `request`
    /// nor `notification`. Imported functions named `wait_for_message` in the Wasm module named
    /// `interface` are an exception and are allowed.
    InvalidImport {
        /// Wasm module name of the offending imported function.
        module: String,
        /// Name of the offending imported function.
        name: String,
    },
    /// An imported function doesn't have the signature expected for its name.
    InvalidImportSignature {
        /// Wasm module name of the offending imported function.
        module: String,
        /// Name of the offending imported function.
        name: String,
    },
}

impl fmt::Display for ModuleBuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleBuilderError::Vm(err) => fmt::Display::fmt(err, f),
            ModuleBuilderError::InvalidImport { module, name } => {
                write!(f, "invalid imported function: {module:?}.{name:?}")
            }
            ModuleBuilderError::InvalidImportSignature { module, name } => {
                write!(
                    f,
                    "invalid signature of imported function: {module:?}.{name:?}"
                )
            }
        }
    }
}

/// A compiled Wasm module whose imported functions are all interface requests or
/// notifications, or waiting for a message. See [`ModuleBuilder`].
///
/// Cloning a `Module` is cheap, as the compiled code is shared between the clones.
pub struct Module<TCodeAlloc: Allocator> {
    /// The compiled module.
    inner: Arc<low_level_vm::Module<TCodeAlloc>>,
    /// Imported functions of the module, ordered by import index.
    imports: Arc<[ModuleImport]>,
}

impl<TCodeAlloc: Allocator> Clone for Module<TCodeAlloc> {
    #[inline]
    fn clone(&self) -> Self {
        Module {
            inner: Arc::clone(&self.inner),
            imports: Arc::clone(&self.imports),
        }
    }
}

/// Function imported by a [`Module`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleImport {
    /// Function named `request` in the Wasm module `interface-<hex>`, where `<hex>` is the
    /// given interface.
    Request {
        /// Interface the request is sent on.
        interface: [u8; 32],
    },
    /// Function named `notification` in the Wasm module `interface-<hex>`, where `<hex>` is
    /// the given interface.
    Notification {
        /// Interface the notification is sent on.
        interface: [u8; 32],
    },
    /// Function named `wait_for_message` in the Wasm module `interface`.
    WaitForMessage,
}

/// Prototype for an instance of a [`Module`].
pub struct InstanceBuilder<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator> {
    /// Module to instantiate.
    module: Module<TCodeAlloc>,
    /// Public key of the instance.
    public_key: [u8; 32],
    /// List of `(interface, handler public key)` registered with
    /// [`InstanceBuilder::register_interface_handler`], in order of registration. Might contain
    /// duplicate interfaces, which are reported by [`InstanceBuilder::build`].
    interface_handlers: Vec<([u8; 32], [u8; 32])>,
    /// Allocator of the native stack of the instance.
    stack_allocator: TStackAlloc,
    /// Allocator of the linear memory of the instance.
    memory_allocator: TMemAlloc,
}

impl<TCodeAlloc: Allocator, TStackAlloc: Allocator, TMemAlloc: Allocator>
    InstanceBuilder<TCodeAlloc, TStackAlloc, TMemAlloc>
{
    /// Initializes a new builder for an instance of the given module, whose public key is
    /// `public_key`.
    ///
    /// The native stack of the instance is allocated with `stack_allocator`, and its linear
    /// memory with `memory_allocator`. See [`low_level_vm::Instance::new`] for the constraints
    /// on these allocators.
    ///
    /// Initially, no interface has a handler.
    #[inline]
    pub fn new(
        module: &Module<TCodeAlloc>,
        public_key: [u8; 32],
        stack_allocator: TStackAlloc,
        memory_allocator: TMemAlloc,
    ) -> Self {
        InstanceBuilder {
            module: module.clone(),
            public_key,
            interface_handlers: Vec::new(),
            stack_allocator,
            memory_allocator,
        }
    }

    /// Registers the program whose public key is `handler_public_key` as the handler of the
    /// messages that the instance sends on `interface`.
    ///
    /// Each interface must be registered at most once, must not be implemented natively by the
    /// executor, and the handler must have already been instantiated in the executor. This is
    /// checked by [`InstanceBuilder::build`]. Registering an interface
    /// that the module doesn't use is allowed.
    #[inline]
    pub fn register_interface_handler(
        &mut self,
        interface: [u8; 32],
        handler_public_key: [u8; 32],
    ) {
        self.interface_handlers
            .push((interface, handler_public_key));
    }

    /// Instantiates the module and pushes the new instance to the queue of instances that are
    /// ready in the given executor. The `main` function of the instance will be called by
    /// [`Executor::run`].
    ///
    /// Returns an error if an interface has been registered multiple times with
    /// [`InstanceBuilder::register_interface_handler`], or if it has been registered while
    /// being implemented natively by the executor (see
    /// [`ExecutorBuilder::register_native_interface`]), or if an interface used by the module
    /// is neither implemented natively nor registered, or if the handler of an interface hasn't
    /// been instantiated in the executor, or if an instance with the same public key has
    /// already been instantiated in the executor. Also returns an error if allocating the
    /// linear memory of the instance fails, or if a data segment of the module doesn't fit in
    /// its linear memory.
    pub fn build(
        self,
        executor: &Executor<TCodeAlloc, TStackAlloc, TMemAlloc>,
    ) -> Result<(), InstanceBuilderError> {
        // The lock is held until the end of the function, so that no other instance with the
        // same public key can be registered in the meantime.
        let mut delivery_queues = executor.delivery_queues.lock();
        if delivery_queues.contains_key(&self.public_key) {
            return Err(InstanceBuilderError::DuplicatePublicKey);
        }

        let mut interface_handlers = hashbrown::HashMap::with_capacity_and_hasher(
            self.interface_handlers.len(),
            fnv::FnvBuildHasher::default(),
        );
        for (interface, handler_public_key) in self.interface_handlers {
            if executor.native_interfaces.contains(&interface) {
                return Err(InstanceBuilderError::NativeInterfaceHandler { interface });
            }
            let Some(queue) = delivery_queues.get(&handler_public_key) else {
                return Err(InstanceBuilderError::UnknownInterfaceHandler {
                    interface,
                    handler_public_key,
                });
            };
            if interface_handlers
                .insert(interface, Arc::clone(queue))
                .is_some()
            {
                return Err(InstanceBuilderError::DuplicateInterfaceHandler { interface });
            }
        }

        for import in self.module.imports.iter() {
            let (ModuleImport::Request { interface } | ModuleImport::Notification { interface }) =
                import
            else {
                continue;
            };
            if !executor.native_interfaces.contains(interface)
                && !interface_handlers.contains_key(interface)
            {
                return Err(InstanceBuilderError::MissingInterfaceHandler {
                    interface: *interface,
                });
            }
        }

        let vm = low_level_vm::Instance::new(
            self.module.inner,
            &executor.trap_handlers,
            self.stack_allocator,
            self.memory_allocator,
        )
        .map_err(InstanceBuilderError::Vm)?;

        let pending_delivery = Arc::new(crossbeam_queue::SegQueue::new());
        delivery_queues.insert(self.public_key, Arc::clone(&pending_delivery));

        executor.ready.push(ReadyInstance {
            instance: Instance {
                vm,
                imports: self.module.imports,
                public_key: self.public_key,
                interface_handlers,
                pending_delivery,
            },
            status: ReadyStatus::NotStarted,
        });

        Ok(())
    }
}

/// Error that can happen in [`InstanceBuilder::build`].
#[derive(Debug)]
pub enum InstanceBuilderError {
    /// The interface has been registered multiple times with
    /// [`InstanceBuilder::register_interface_handler`].
    DuplicateInterfaceHandler {
        /// The offending interface.
        interface: [u8; 32],
    },
    /// The interface has been registered with [`InstanceBuilder::register_interface_handler`],
    /// but is implemented natively by the executor.
    NativeInterfaceHandler {
        /// The offending interface.
        interface: [u8; 32],
    },
    /// The module uses an interface that is neither implemented natively by the executor nor
    /// registered with [`InstanceBuilder::register_interface_handler`].
    MissingInterfaceHandler {
        /// The offending interface.
        interface: [u8; 32],
    },
    /// The program registered with [`InstanceBuilder::register_interface_handler`] as the
    /// handler of the interface hasn't been instantiated in the executor.
    UnknownInterfaceHandler {
        /// The offending interface.
        interface: [u8; 32],
        /// Public key of the handler that hasn't been instantiated.
        handler_public_key: [u8; 32],
    },
    /// An instance with the same public key has already been instantiated in the executor.
    DuplicatePublicKey,
    /// Failed to instantiate the module.
    // TODO: don't expose low_level_vm::Error
    Vm(low_level_vm::Error),
}

impl fmt::Display for InstanceBuilderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (message, interface) = match self {
            InstanceBuilderError::DuplicateInterfaceHandler { interface } => {
                ("interface handler registered multiple times", interface)
            }
            InstanceBuilderError::NativeInterfaceHandler { interface } => (
                "interface handler registered for a native interface",
                interface,
            ),
            InstanceBuilderError::MissingInterfaceHandler { interface } => {
                ("no handler for interface", interface)
            }
            InstanceBuilderError::UnknownInterfaceHandler {
                interface,
                handler_public_key,
            } => {
                write!(f, "handler of interface ")?;
                for byte in interface {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, " not instantiated: ")?;
                for byte in handler_public_key {
                    write!(f, "{byte:02x}")?;
                }
                return Ok(());
            }
            InstanceBuilderError::DuplicatePublicKey => {
                return write!(f, "instance with the same public key already exists");
            }
            InstanceBuilderError::Vm(err) => return fmt::Display::fmt(err, f),
        };

        write!(f, "{message}: ")?;
        for byte in interface {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
