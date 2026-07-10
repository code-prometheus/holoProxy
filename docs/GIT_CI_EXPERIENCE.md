# Rust 项目 GitHub Actions CI/CD 实战总结

> **目标读者**：用 Rust 构建 Windows 可执行文件、需要通过 CI 自动编译测试打包发布到 GitHub Release 的开发者。本文覆盖了从零搭建到踩坑修复的完整经验。

---

## 1. 最小可用 CI 模板

以下是一个 Rust Windows 项目的完整 CI 配置，直接复制即可用：

```yaml
name: CI

on:
  push:
    branches: [main]
    tags: ['v*']          # 推送 v1.0.0 等 tag 时触发 release
  pull_request:
    branches: [main]

permissions:
  contents: write          # 创建 Release 需要写权限
  actions: read            # download-artifact 需要读权限

env:
  CARGO_TERM_COLOR: always

jobs:
  build-and-test:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4

      - uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: ${{ runner.os }}-cargo-

      - name: Build
        run: cargo build --release

      - name: Test
        run: cargo test --release

      - name: Package
        shell: pwsh
        run: |
          mkdir artifact
          copy target/release/my_app.exe artifact/
          copy settings.json artifact/
          Compress-Archive -Path artifact/* -DestinationPath my_app-windows-x64.zip

      - uses: actions/upload-artifact@v4
        with:
          name: my_app-windows-x64
          path: my_app-windows-x64.zip

  release:
    if: startsWith(github.ref, 'refs/tags/v')
    needs: build-and-test
    runs-on: windows-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          name: my_app-windows-x64

      - name: Create GitHub Release
        shell: bash
        run: |
          gh release create "${{ github.ref_name }}" my_app-windows-x64.zip \
            --title "my_app ${{ github.ref_name }}" \
            --generate-notes \
            --repo "${{ github.repository }}"
        env:
          GH_TOKEN: ${{ github.token }}
```

---

## 2. 核心概念

### 2.1 触发条件

```yaml
on:
  push:
    branches: [main]    # 每次 push 到 main 就编译测试
    tags: ['v*']        # 推送 v0.1.0 等 tag 时额外触发 release
  pull_request:
    branches: [main]    # PR 也编译测试（但不 release）
```

### 2.2 权限

```yaml
permissions:
  contents: write       # gh release create 需要
  actions: read         # download-artifact 跨 job 读取需要
```

**坑**：不加 `permissions` 的话，默认 `GITHUB_TOKEN` 只有只读权限，`gh release create` 会失败。

### 2.3 两个 Job 的职责

| Job | 触发条件 | 做什么 |
|-----|---------|--------|
| `build-and-test` | 每次 push / PR | 编译 → 测试 → 打包 → 上传 artifact |
| `release` | 仅 tag push | 下载 artifact → 用 `gh` CLI 创建 GitHub Release |

`release` 通过 `needs: build-and-test` 等待编译完成，通过 `if: startsWith(github.ref, 'refs/tags/v')` 只 在 tag 时运行。

---

## 3. 缓存策略

```yaml
- uses: actions/cache@v4
  with:
    path: |
      ~/.cargo/registry   # 下载的 crate 源码
      ~/.cargo/git        # git 依赖
      target              # 编译产物
    key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
    restore-keys: ${{ runner.os }}-cargo-
```

**关键点**：
- cache key 用 `Cargo.lock` 的 hash — 依赖变了 key 就变，自动重新编译
- `restore-keys` 是 fallback：相同 OS 的旧缓存也能用，只是可能多编几个 crate
- **`Cargo.lock` 必须提交到 git**（可执行项目！），否则 hash 每次都不一样

**效果**：首次 CI ~2min50s，后续缓存命中 ~1min30s。

---

## 4. 打包策略

```yaml
- name: Package
  shell: pwsh
  run: |
    mkdir artifact
    copy target/release/my_app.exe artifact/
    copy settings.json artifact/          # 配置文件
    copy assets/icon.ico artifact/assets/ # 资源文件
    Compress-Archive -Path artifact/* -DestinationPath my_app-windows-x64.zip
```

- 用 PowerShell（`shell: pwsh`）因为 `Compress-Archive` 是 Windows 原生命令
- artifact 目录里放 exe + 配置文件 + 资源文件，用户解压即用
- zip 文件名对应平台（`windows-x64`），方便多平台扩展

---

## 5. 发布 Release（核心踩坑）

### 5.1 最终可行方案：`gh` CLI

```yaml
- name: Create GitHub Release
  shell: bash
  run: |
    gh release create "${{ github.ref_name }}" holoProxy-windows-x64.zip \
      --title "holoProxy ${{ github.ref_name }}" \
      --generate-notes \
      --repo "${{ github.repository }}"
  env:
    GH_TOKEN: ${{ github.token }}
```

- `${{ github.ref_name }}` = tag 名（如 `v0.1.8`）
- `--generate-notes` = 自动从 commits 生成 release notes
- `${{ github.token }}` = 内置 token，无需配 secrets
- `GH_TOKEN` 环境变量（不是 `GITHUB_TOKEN`！`gh` CLI 认 `GH_TOKEN`）

### 5.2 失败方案：`softprops/action-gh-release@v2`

```yaml
# ❌ 不要用这个，看似简单但极其难调试
- uses: softprops/action-gh-release@v2
  with:
    files: my_app-windows-x64.zip
```

**为什么失败**：这个 action 不报错、不输出日志、静默失败。可能是 token 格式、artifact 路径、权限等问题，但你永远不知道具体原因。**直接用 `gh` CLI**，输出清晰。

### 5.3 env 放错位置（YAML 缩进坑）

```yaml
# ❌ 错误：env 被放在 with 里面了（step 级 env 应该在 with 同级）
- uses: some-action@v1
  with:
    key: value
    env:              # ← 这里 env 被当作 with 的参数，不是 step 级 env
      TOKEN: xxx

# ✅ 正确：env 跟 uses/with 同级
- uses: some-action@v1
  with:
    key: value
  env:                # ← step 级 env，action 能通过环境变量读取
    TOKEN: xxx
```

---

## 6. 常见问题排查

| 症状 | 原因 | 修复 |
|------|------|------|
| `cargo build` 很慢 | 没缓存 | 加 `actions/cache@v4` |
| 缓存不命中 | `Cargo.lock` 没提交 | 把 `Cargo.lock` 从 `.gitignore` 移除 |
| Release 不创建 | 没打 tag | 只有 tag push 才触发 release job |
| Release 不创建 | 权限不够 | 加 `permissions: contents: write` |
| `gh release create` 报权限错 | `env` 写错 | 用 `GH_TOKEN` 不是 `GITHUB_TOKEN` |
| artifact 找不到 | `name` 不匹配 | upload/download 的 `name` 必须一致 |
| zip 里文件不全 | `Compress-Archive` 路径错 | `-Path artifact/*` 不是 `-Path artifact` |
| Windows runner 慢 | 正常现象 | 首次编译 2-3 分钟，缓存后 1-2 分钟 |
| 多个 CI 同时跑 | push + tag 同时触发 | 正常，main 和 tag 各跑一个，互不干扰 |

---

## 7. tag 发布工作流

每次要发新版时：

```bash
# 1. 确认所有改动已提交
git status

# 2. 推送到 main（触发普通 CI，只编译测试不 release）
git push

# 3. 打 annotated tag 并推送（触发 release）
git tag v0.1.9 -m "v0.1.9: 简短描述"
git push origin v0.1.9
```

等 2-3 分钟，去 `https://github.com/你的用户名/你的仓库/releases` 就能看到新版本 zip 下载。

---

## 8. build.rs 资源复制配合 CI

如果你的项目用 `build.rs` 在编译时复制资源文件到 `target/` 目录，CI 里记得把这些文件也打进 artifact：

```rust
// build.rs
fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let target_dir = Path::new(&out_dir).ancestors().nth(3).unwrap();

    // 复制 assets/
    copy_dir("assets", &target_dir.join("assets")).ok();

    // 复制 settings.json
    std::fs::copy("settings.json", target_dir.join("settings.json")).ok();
}
```

然后 CI 的 Package 步骤里：

```yaml
copy target/release/my_app.exe artifact/
copy settings.json artifact/              # 从项目根目录，不是 target
copy assets/icon.ico artifact/assets/     # 从项目根目录
```

---

## 9. settings.json 不应该进 git（敏感信息时）

如果 settings.json 含 API key 等敏感信息：

```yaml
# .gitignore
settings.json
```

CI 的 Package 步骤里就不 copy 了，用户自己配置。或者提供一个 `settings.example.json` 模板。

---

## 10. 总结

| 要点 | 做法 |
|------|------|
| CI 平台 | GitHub Actions，`windows-latest` runner |
| 编译 | `cargo build --release` |
| 测试 | `cargo test --release`（--release 确保和发布的 exe 一致） |
| 缓存 | `actions/cache@v4`，key 用 `Cargo.lock` 的 hash |
| 打包 | PowerShell `Compress-Archive` |
| Release | `gh release create` CLI，不用第三方 action |
| 权限 | `contents: write` + `actions: read` |
| Token | `${{ github.token }}` 设到 `GH_TOKEN` 环境变量 |
| 触发 | push main → 编译测试；push tag v* → 编译测试 + Release |
