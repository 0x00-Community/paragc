# paragc 之书

[English](README.md) · **简体中文**

[paragc](https://github.com/0x00-Community/paragc) 的完整讲解，用 [mdBook](https://rust-lang.github.io/mdBook/) 构建，作为
Cloudflare Worker（静态资源）部署在 <https://paragc.app238.com>。

| 路径 | |
|---|---|
| `en/` | 英文版（`en/src/SUMMARY.md`），站点根 |
| `zh-CN/` | 中文译本，在 `/zh-CN/` 下 |
| `en/lang.js` | 菜单栏的语言切换（`zh-CN/lang.js` 链接到它） |
| `wrangler.jsonc` | Worker：发布 `book/`，未知路径用 mdBook 的 `404.html` |

```sh
bun install                 # wrangler
bun run build               # mdbook build en && mdbook build zh-CN ⇒ book/
bun run serve               # 英文版实时预览（中文版用 serve:zh）
bun run dev                 # 构建后 wrangler dev
bun run deploy              # 构建后 wrangler deploy
```

`build.sh` 优先用 PATH 里的 `mdbook`，没有就下载钉住的 0.5.4。手动部署时如果有多个 Cloudflare 账号，用 `CLOUDFLARE_ACCOUNT_ID`
选一个。

`en/src/bits.md`、`en/src/moving.md` 及对应的中文页由 `diagrams.py` 生成（HTML 画图，样式在 `en/diagrams.css`）：改脚本后运行 `python3 diagrams.py .`。

英文是原文：改 `en/src/` 的某一页时，同时改 `zh-CN/src/` 的同一页。
