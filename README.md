<p align="center">
    <img width="200" alt="Kairos Logo" src="https://raw.githubusercontent.com/kairos/kairos/master/extra/logo/compat/kairos-term%2Bscanlines.png">
</p>

<h1 align="center">Kairos - A fast, cross-platform, OpenGL terminal emulator</h1>

<p align="center">
  <img alt="Kairos - A fast, cross-platform, OpenGL terminal emulator"
       src="https://raw.githubusercontent.com/kairos/kairos/master/extra/promo/kairos-readme.png">
</p>

## About

Kairos is a modern terminal emulator that comes with sensible defaults, but
allows for extensive [configuration](#configuration). By integrating with other
applications, rather than reimplementing their functionality, it manages to
provide a flexible set of [features](./docs/features.md) with high performance.
The supported platforms currently consist of BSD, Linux, macOS and Windows.

The software is considered to be at a **beta** level of readiness; there are
a few missing features and bugs to be fixed, but it is already used by many as
a daily driver.

Precompiled binaries are available from the [GitHub releases page](https://github.com/kairos/kairos/releases).

Join [`#kairos`] on libera.chat if you have questions or looking for a quick help.

[`#kairos`]: https://web.libera.chat/gamja/?channels=#kairos

## Features

You can find an overview over the features available in Kairos [here](./docs/features.md).

## Further information

- [Announcing Kairos, a GPU-Accelerated Terminal Emulator](https://jwilm.io/blog/announcing-kairos/) January 6, 2017
- [A talk about Kairos at the Rust Meetup January 2017](https://www.youtube.com/watch?v=qHOdYO3WUTk) January 19, 2017
- [Kairos Lands Scrollback, Publishes Benchmarks](https://jwilm.io/blog/kairos-lands-scrollback/) September 17, 2018

## Installation

Kairos can be installed by using various package managers on Linux, BSD,
macOS and Windows.

Prebuilt binaries for macOS and Windows can also be downloaded from the
[GitHub releases page](https://github.com/kairos/kairos/releases).

For everyone else, the detailed instructions to install Kairos can be found
[here](INSTALL.md).

### Requirements

- At least OpenGL ES 2.0
- [Windows] ConPTY support (Windows 10 version 1809 or higher)

## Configuration

You can find the documentation for Kairos's configuration in `man 5
kairos`, or by looking at [the website] if you do not have the manpages
installed.

[the website]: https://kairos.org/config-kairos.html

Kairos doesn't create the config file for you, but it looks for one in the
following locations:

1. `$XDG_CONFIG_HOME/kairos/kairos.toml`
2. `$XDG_CONFIG_HOME/kairos.toml`
3. `$HOME/.config/kairos/kairos.toml`
4. `$HOME/.kairos.toml`
5. `/etc/kairos/kairos.toml`

On Windows, the config file will be looked for in:

* `%APPDATA%\kairos\kairos.toml`

## Contributing

A guideline about contributing to Kairos can be found in the
[`CONTRIBUTING.md`](CONTRIBUTING.md) file.

## FAQ

**_Is it really the fastest terminal emulator?_**

Benchmarking terminal emulators is complicated. Kairos uses
[vtebench](https://github.com/kairos/vtebench) to quantify terminal emulator
throughput and manages to consistently score better than the competition using
it. If you have found an example where this is not the case, please report a
bug.

Other aspects like latency or framerate and frame consistency are more difficult
to quantify. Some terminal emulators also intentionally slow down to save
resources, which might be preferred by some users.

If you have doubts about Kairos's performance or usability, the best way to
quantify terminal emulators is always to test them with **your** specific
usecases.

**_Why isn't feature X implemented?_**

Kairos has many great features, but not every feature from every other
terminal. This could be for a number of reasons, but sometimes it's just not a
good fit for Kairos. This means you won't find things like tabs or splits
(which are best left to a window manager or [terminal multiplexer][tmux]) nor
niceties like a GUI config editor.

[tmux]: https://github.com/tmux/tmux

## License

Kairos is released under the [Apache License, Version 2.0].

[Apache License, Version 2.0]: https://github.com/kairos/kairos/blob/master/LICENSE-APACHE
