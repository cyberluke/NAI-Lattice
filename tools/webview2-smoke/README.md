# webview2-smoke (V03)

Real WebView2 capture harness for the Lattice dashboard preview. It loads a URL
in an actual Microsoft Edge WebView2 browser (the same engine NAI OS embeds),
waits for `CoreWebView2.NavigationCompleted` plus a settle delay, captures the
rendered viewport with `CoreWebView2.CapturePreviewAsync(Png)`, and prints one
JSON line. It never injects script into the page and never requires interaction.

## Requirements

- Windows 10/11 with the **WebView2 Evergreen Runtime** installed.
- .NET SDK 11 (`net11.0-windows`, WinForms). Any SDK that can target
  `net11.0-windows` works; adjust `TargetFramework` in the `.csproj` for an
  older installed SDK.
- NuGet access on the first restore (`Microsoft.Web.WebView2` 1.0.4370.26).

## Build

```powershell
dotnet build tools\webview2-smoke\WebView2Smoke.csproj -c Release
```

Output (framework-dependent, win-x64):

```text
tools\webview2-smoke\bin\Release\net11.0-windows\win-x64\webview2-smoke.exe
```

## Run

```powershell
tools\webview2-smoke\bin\Release\net11.0-windows\win-x64\webview2-smoke.exe `
  --url http://127.0.0.1:8793/lattice/next `
  --out tools\webview2-smoke\out\webview2-dashboard.png `
  --wait-ms 5000
```

Or via the SDK:

```powershell
dotnet run --project tools\webview2-smoke\WebView2Smoke.csproj -c Release -- `
  --url http://127.0.0.1:8793/lattice/next `
  --out tools\webview2-smoke\out\webview2-dashboard.png `
  --wait-ms 5000
```

### Arguments

| Argument | Required | Meaning |
|---|---|---|
| `--url <url>` | yes | Absolute `http(s)` URL to load. |
| `--out <png>` | yes | Output PNG path (parent directories are created). |
| `--wait-ms <n>` | no | Settle delay after navigation completes; default `5000`, `0..120000`. |
| `--user-data-dir <dir>` | no | WebView2 profile directory; default `%TEMP%\nai-webview2-smoke`. |

## Result

One JSON line on stdout:

```json
{"ok":true,"title":"Lattice","png":"D:\\...\\out\\webview2-dashboard.png","pixels":{"width":1600,"height":1000},"dpi":{"x":96,"y":96},"url":"http://127.0.0.1:8793/lattice/next","bytes":123456}
```

- Exit `0`: capture written; `ok` is `true`.
- Exit `2`: argument error, navigation failure/timeout, capture failure, or a
  missing WebView2 runtime. `ok` is `false` and `error` names the cause; no PNG
  is claimed.

`pixels` and `dpi` are measured from the real captured bitmap and the window's
DPI, never assumed.

### Verified run (2026-11-25)

Against the isolated QA host on port 8793 (`/lattice/next`, dashboard 0.1.0):

```text
webview2-smoke.exe --url http://127.0.0.1:8793/lattice/next --out tools\webview2-smoke\out\webview2-dashboard.png --wait-ms 5000
{"ok":true,"title":"NAI Lattice \u00B7 Observability","png":"D:\\_SATIN_AI\\NAI-Lattice\\tools\\webview2-smoke\\out\\webview2-dashboard.png","pixels":{"width":1600,"height":1000},"dpi":{"x":192,"y":192},"url":"http://127.0.0.1:8793/lattice/next","bytes":272318}
```

The captured PNG shows the real Dockview shell (summary cards, log tail, health,
error intelligence with measured rows), not a blank or placeholder frame.

## Notes

- The harness shows an ordinary non-activating window for a few seconds.
  Chromium throttles fully offscreen/occluded windows, which can make
  `CapturePreviewAsync` return a stale or blank frame, so an on-screen window is
  used deliberately.
- The WebView2 user data folder is outside the repository by default so a smoke
  run never dirties the working tree.
- This project is intentionally outside the Rust workspace and is not built by
  `cargo` or the delivery packaging.
