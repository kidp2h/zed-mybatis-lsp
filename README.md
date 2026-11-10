# MyBatis LSP for Zed

> **Credits:** this is a fork of
> [zhangguangbin007/zed-mybatis-lsp](https://github.com/zhangguangbin007/zed-mybatis-lsp)
> by **xqd-java-ad**. All original work belongs to the original author; this
> fork only adds the changes listed below.
>
> **Changes in this fork (0.2.0)**
> - Go to Definition from `<include refid="...">` to the matching `<sql id="...">`
>   (local id or fully qualified `namespace.id`).
> - Find All References from a `<sql id>` (or a `refid`) to every `<include>` using it.
> - Workspace indexing skips `target`, `build`, `node_modules`, `.git` and `.idea`.
> - Unit tests use drive-letter file URLs so they pass on Windows.
> - Release assets are downloaded from `kidp2h/zed-mybatis-lsp`.

This standalone Rust language server adds MyBatis Mapper XML navigation to Zed.

It resolves the conventional mapping used by this repository:

```text
Java Mapper fully-qualified name == <mapper namespace="...">
Java method name              == SQL tag id
```

Supported SQL tags are `select`, `insert`, `update`, `delete`, and `sql`.

## Build the language server

Install Rust with the rsproxy mirror if it is not already available:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://rsproxy.cn/rustup-init.sh |
  RUSTUP_DIST_SERVER=https://rsproxy.cn \
  RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
  sh -s -- -y --default-toolchain stable --profile minimal
```

Then run:

```bash
cd zed-mybatis-lsp
cargo build --release
```

The server binary is `target/release/mybatis-lsp`.

The project uses the rsproxy sparse registry through `.cargo/config.toml`.
For local development on macOS or Linux, run:

```bash
./install-local.sh
```

This installs the binary as `~/.local/bin/mybatis-lsp`.

## Publish for other users

The Zed extension downloads a native server on first startup. The release must
contain these assets with exactly these names:

```text
mybatis-lsp-darwin-aarch64
mybatis-lsp-darwin-x86_64
mybatis-lsp-linux-aarch64
mybatis-lsp-linux-x86_64
mybatis-lsp-windows-x86_64.exe
```

1. `RELEASE_REPOSITORY` in `extension/src/lib.rs` and `repository` in
   `extension/extension.toml` point to `kidp2h/zed-mybatis-lsp`; change them if
   you publish from another repository.
2. Push a `v0.2.0` tag. The `Release` workflow builds the five binaries and
   attaches them to the GitHub release.
3. Build the extension WASM:

```bash
cargo build --manifest-path extension/Cargo.toml \
  --target wasm32-wasip2 --release
cp extension/target/wasm32-wasip2/release/mybatis_navigation.wasm \
  extension/extension.wasm
```

4. Commit and push `extension/extension.toml` and `extension/extension.wasm`.
   For every new server release, increment both `RELEASE_VERSION` in
   `extension/src/lib.rs` and `version` in `extension/extension.toml`, rebuild
   the WASM, and publish the matching `v<version>` release.

## Install for other users

Users only need Zed and a network connection to GitHub:

1. In Zed, open the command palette and run `zed: install dev extension`.
2. Select the `extension` directory from this project (or install the
   extension from the published Zed extension registry once it is registered).
3. Add `mybatis-lsp` to the Java and XML language-server lists if it is not
   already enabled, then restart the language server.

The first startup downloads the platform-specific binary into Zed's extension
directory. No JDK, Maven, `install-local.sh`, or `PATH` setup is required.

## Zed configuration

Add the following to `~/.config/zed/settings.json` (or the project's
`.zed/settings.json`):

```json
{
  "languages": {
    "Java": {
      "language_servers": ["mybatis-lsp", "..."]
    },
    "XML": {
      "language_servers": ["mybatis-lsp", "..."]
    }
  },
  "lsp": {
    "mybatis-lsp": {
      "binary": {
        "path": "/absolute/path/to/zed-mybatis-lsp/target/release/mybatis-lsp"
      }
    }
  }
}
```

For local development, install the companion extension by running
`zed: install dev extension` from the command palette and selecting the
`zed-mybatis-lsp/extension` directory. The extension uses `mybatis-lsp` from
`PATH` when available and otherwise downloads the matching release asset.

## Navigation

- From a Mapper method call or declaration in Java, use **Go to Definition** to open the XML SQL tag.
- From an XML SQL `id`, use **Go to Definition** to open the Java Mapper method.
- From `<include refid="...">`, use **Go to Definition** to open the `<sql>` fragment.
- From a `<sql id>` or a `refid`, use **Find All References** to list every `<include>` of it.

The server watches open documents through LSP notifications and rescans the
workspace on initialization. It intentionally uses lightweight source parsing;
Java compilation is not required.
