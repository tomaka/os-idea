This repository contains a design document for the operating system I wish existed.

# Background

Everyone is tired of dealing with routing issues, certificates, dependency hell, softwares that have to speak dozens of different protocols and API in order to be compatible with multiple different softwares.

Self-hosting servers shouldn't be that hard. Cloud companies earn a shitton money and have control over your data simply because nobody wants to deal with the complexity of self-hosting modern software stacks. This complexity is absolutely insane, and I don't understand why nobody sees this as a fundamental problem in IT in 2026. Hundreds of thousands of people throughout the world are employed just to deal with the consequences of complexity created decades ago and that nobody dared touch.

"Everything is a file" was a major mistake in the history of IT. The "metadata" of a UNIX file (is the read blocking or not? do data losses occur? is there buffering?) is so leaky that it makes the idea of using files as an abstraction very bad. Data in general should be more strongly typed (e.g. configuration, database, cache, ... kind of like XDG stuff), and this should be enforced at the operating system level, rather than just having a one-size-fits-all concept of "file". Files can still exist, but not as a core building block.

Usernames and password are legacy. Everything should use public-key architecture. There is no justification why you in 2026 you for example connect to a server via HTTPS then have to provide a password afterwards, while your TLS public key is already your identifier.

The concept of TCP and UDP ports is legacy as they're just meaningless numbers, through of course we're not going to get rid of TCP/UDP any time soon.

Having to use Docker or similar in order to properly isolate programs from each other is ridiculous. Programs should not be sharing a file system with each other by default, and limiting the memory and CPU of a program should be trivial.

Routing is way too difficult. IPv6 is for some reason still not a thing in 2026, and we're stuck in the shitty world of NATs. Kubernetes creates private networks between containers just to solve this problem. Good luck creating a Kubernetes cluster with non-publicly-accessible machines.

Programs loading their configuration from the file system is legacy. All the configuration of a program should be passed by something equivalent to CLI args or environment variables, preferably with something like JSON or something. This makes programs more pure, and avoids problems like the impossibility to run two instances of the same program with different configurations, or not knowing where the configuration file is located, or having to manually restart the program after modifying the configuration file.

Shared libraries are legacy. In the era of gigabytes of RAM and terabytes of storage, nobody cares about a few megabytes saved. Again, hundreds or thousands of people spend their days managing distribution packages and trying to make sure that nothing breaks. Software developers of core software never make any change that could break backwards compatibility, which in turn leads to decades of legacy stuff. Every program being self contained makes distributing software a few degrees of magnitude easier.

# Concepts

- **Program binary**: Similar to how Linux has ELF files and Windows has PE files. In this OS, binaries are referenced purely by their hash, similar to a Docker image hash for instance. The ABI of a program binary should be cross-platform, such as WebAssembly.
- **Program instance**: When a program is launched for the first time, a private key is generated. Instead of having a PID, each program is identified by its public key. If the program restarts or updates, its key is maintained.
- **Interface**: Programs are by default completely CPU-only and have no access to any I/O (no files, no networking, no randomness, no time, etc.). In order to do any I/O whatsoever, a program must send a message to a different program. When sending a message, the program doesn't specify the recipient of the message but simply the relevant interface. Example interfaces include: randomness, time, tcp/ip, etc. Which program is the recipient of the message is decided externally by whoever launched the program. TODO: how are interfaces identified? by hash of a document? Each program binary has, as part of its metadata, a list of interfaces that it implements and a list of interfaces that it uses. The links between programs must be determined when the program is launched. For each interface that a program implements, it is possible to specify which programs have access to it (or allow any program).
- **Message**: A program can send a message over an interface. There are two kinds of messages: request-responses, and notifications. Notifications can get silently dropped; if you want a notification with a confirmation of reception, you should use a request-response and send back an empty response. Contrary to what TCP does, responses don't have to come in the same order as the requests. These two kind of messages are not just industry standard, but they are fundamentally how data flows work in theory in computer science.

# In practice

Standard interfaces:

- **Artifacts provider**: Registers a function. This function can later be called with an artifact hash and returns the binary. The implementation of this interface is a "server", while users of this interface are the actual providers.
- **Executor**: Starts and stops other programs.
- **Message tunneling**: Can be notified of the presence of "external programs" that implement a specific interface. In order to send a message to one of these "external programs", a function is registered. Messages are encrypted by the implementer of the interface (i.e. ahead of the program doing the sending).
- **TCP/IP**: Open TCP or UDP connections. Used by the core system, but shouldn't be used directly by programs unless when interacting with legacy software.
- **Link**: Opens and maintains links to other programs identified by their public key.
- **Cache**: Key-value store, where entries can disappear spontaneously.
- other...

Typical programs:

- The "local executor" is probably the most important program. It implements **executor**, **message tunneling**, and **artifacts-provider**, and makes it possible to start programs on the local machine. Its **executor** interface is typically only allowed access from the equivalent of the local shell or desktop environment program. The equivalent of SSH can be done by allowing access from remote programs.
- A cache of artifacts on the file system of the local machine. It uses the **artifacts provider** provider to register itself but also implements the **artifacts provider** interface.   TODO: maybe this registration thing should be a native mechanism, otherwise we have to wait for the registration to happen, which takes an unknown amount of time and is thus a very hacky mechanism
- A program that loads artifacts from a server on the Internet and uses the **artifacts provider** to register itself.
- A general-purpose on-disk cache provider.
- The network stack. Implements the **TCP/IP** interface.
- The relay. It uses the **TCP/IP** interface to discover other machines (TODO: discovery not designed), and the **message tunneling** interface to notify the local system of other programs that it discovers.
- The cluster agent. It registers itself towards a central program using the **cluster-orchestrator** interface, and uses **executor** to spawn the programs that have been requested. Its configuration possibly adds CPU and memory limits.
- The cluster orchestrator. It implements the **cluster-orchestrator** interface and the **executor** interface, and spawns the programs on its cluster agents.

This is a demonstration of the concept. Overall you'd also have things like USB-related, HID-related, or graphical-related interfaces/programs for example.

# Implementation

Because it is realistically not possible to reimplement all necessary hardware drivers, the OS should be built on top of the Linux kernel, but **as an implementation detail**. That is, it must not be possible to determine whatsoever that Linux is running underneath (except of course if you're extracting the harddrive of the machine and looking at its content).
Future implementations could in principle be built more natively.

For message passing to be efficient, we kind of need a way to pass blobs between isolated processes without copying their content. This is normally easy to do with virtual memory/paging, but WebAssembly has no way to do that at the moment, and the committee is taking a long time to design things. Maybe WebAssembly isn't a good choice for this reason.

# How interfaces work

Two types of messages: requests, and notifications. Each request expects a single answer.

Format of messages in CBOR.

Requests include an opaque 64bits number that serves as identifier and is passed back in the response. In principle this could be an opaque piece of data, but it is good to guarantee that the recipient doesn't have to dynamically allocate memory to store these numbers.

Communication is intentionally unidirectional. The recipient can never request things from the sender. Things like callbacks are implemented by having the sender send requests and the recipient answers these requests only when necessary.
For example in order to read from a socket, you send "read" requests and the TCP code answers requests one after the other every time some data arrives on the socket.

When it comes to the actual message mechanism, if we want something not terribly slow we need to use Wasm shared memory.
We need a syscall that tells the kernel to open an IPC with a given destination, passing as parameter a pointer in memory. From the point of view of the Wasm program, it is as if this spawned a thread that reads/writes this memory.
What the kernel does is share the memory with the other program.
Then the two programs communicate with atomics.

If a program is talking to several different other programs, they each individually have their own buffer of messages. Otherwise programs could corrupt each other.

TODO: need to handle sleeping
TODO: is it not problematic to have many different queues
