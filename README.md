# `redo`

Another implementation of [DJB's `redo`](https://cr.yp.to/redo.html).
Inspired by the great implementation by [`apenwarr`](https://github.com/apenwarr/redo).

*Differences/Features*

- Cross Platform: works on Windows, WSL within Windows, Linux, MacOS.
- Ability to better handle when targets have been manually edited.
- Slightly different, but robust, change detection mechanisms from the `apenwarr` implementation.
- Written in Rust(?)

> [!WARNING]
> This was built in large part with Claude/Codex. Feel free to draw your own conclusion with that information.

## Use in GitHub Actions

```yaml
- uses: mitchpaulus/redo-msh@v0.1.0
```

Installs the release binaries matching the pinned tag (`redo`, `redo-ifchange`, `redo-always`, `redo-ifcreate`, `redo-stamp`) and adds them to `PATH`. Linux x86_64, macOS arm64, and Windows x86_64 runners are supported.
