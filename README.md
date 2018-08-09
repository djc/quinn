# Quinn

[![Build status](https://api.travis-ci.org/djc/quinn.svg?branch=master)](https://travis-ci.org/djc/quinn)
[![codecov](https://codecov.io/gh/djc/quinn/branch/master/graph/badge.svg)](https://codecov.io/gh/djc/quinn)
[![Chat](https://badges.gitter.im/gitterHQ/gitter.svg)](https://gitter.im/djc/quinn)

Quinn is an implementation of the [QUIC][quic] protocol based on futures.
It is currently in its very early stages: work is ongoing on implementing
the protocol handshake for both client and server.

Quinn is the subject of a [RustFest Paris (May 2018) presentation][talk]; you can also get
the [slides][slides] (and the [animation][animation] about head-of-line blocking).
Video of the talk is available [on YouTube][youtube].

All feedback welcome. Feel free to file bugs, requests for documentation and
any other feedback to the [issue tracker][issues] or [tweet me][twitter].

Quinn was created by Dirkjan Ochtman. If you are in a position to support further
development or want to use it in your project, please consider supporting my open
source work on [Patreon][patreon].

# Features

* Currently targets QUIC draft 11 and TLS 1.3 draft 28
* Uses [rustls][rustls] for all TLS operations
* Uses [*ring*][ring] for AEAD encryption/decryption

[quic]: https://quicwg.github.io/
[issues]: https://github.com/djc/quinn/issues
[twitter]: https://twitter.com/djco/
[rustls]: https://github.com/ctz/rustls
[ring]: https://github.com/briansmith/ring
[talk]: https://paris.rustfest.eu/sessions/a-quic-future-in-rust
[slides]: https://dirkjan.ochtman.nl/files/quic-future-in-rust.pdf
[animation]: https://dirkjan.ochtman.nl/files/head-of-line-blocking.html
[youtube]: https://www.youtube.com/watch?v=EHgyY5DNdvI
[patreon]: https://www.patreon.com/dochtman
