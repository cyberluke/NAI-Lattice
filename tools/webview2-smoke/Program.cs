// Real WebView2 capture harness (V03).
//
// Loads a URL in an actual Microsoft Edge WebView2 browser, waits for the
// navigation to finish (plus an explicit settle delay), captures the rendered
// viewport as a PNG through CoreWebView2.CapturePreviewAsync, and prints one
// JSON line describing the result. No interaction is required and no script is
// injected into the page.
//
// Exit codes: 0 = captured, 2 = any failure (argument, navigation, capture,
// missing WebView2 runtime). The PNG is only reported on success.

using System.Drawing;
using System.Text.Json;
using Microsoft.Web.WebView2.Core;
using Microsoft.Web.WebView2.WinForms;

namespace WebView2Smoke;

internal static class Program
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        WriteIndented = false,
    };

    [STAThread]
    private static int Main(string[] args)
    {
        Options options;
        try
        {
            options = Options.Parse(args);
        }
        catch (ArgumentException ex)
        {
            Emit(new Dictionary<string, object?> { ["ok"] = false, ["error"] = ex.Message });
            return 2;
        }

        // Per-monitor DPI so the captured bitmap and reported DPI are real.
        Application.SetHighDpiMode(HighDpiMode.PerMonitorV2);
        Application.EnableVisualStyles();
        Application.SetCompatibleTextRenderingDefault(false);

        // Application.Run pumps the WinForms message loop; the async workflow
        // starts on Shown and closes the form when it is done. This is required
        // for WebView2's async continuations (no manual DoEvents loop).
        using var form = new SmokeForm(options);
        Application.Run(form);

        Emit(form.Result);
        return form.Result.TryGetValue("ok", out var ok) && ok is true ? 0 : 2;
    }

    private static void Emit(Dictionary<string, object?> result)
    {
        Console.Out.WriteLine(JsonSerializer.Serialize(result, JsonOptions));
        Console.Out.Flush();
    }
}

internal sealed class SmokeForm : Form
{
    private const int NavigationTimeoutMs = 30_000;

    private readonly Options _options;
    private readonly WebView2 _webView;

    public Dictionary<string, object?> Result { get; private set; } = new()
    {
        ["ok"] = false,
        ["error"] = "harness did not run",
    };

    public SmokeForm(Options options)
    {
        _options = options;
        Text = "NAI Lattice WebView2 smoke";
        ClientSize = new Size(1600, 1000);
        StartPosition = FormStartPosition.Manual;
        Location = new Point(40, 40);
        ShowInTaskbar = false;

        _webView = new WebView2 { Dock = DockStyle.Fill };
        Controls.Add(_webView);
        Shown += OnShown;
    }

    // An ordinary visible window that never steals focus; Chromium throttles
    // fully occluded/offscreen windows, which would make CapturePreview show a
    // stale or blank frame.
    protected override bool ShowWithoutActivation => true;

    private async void OnShown(object? sender, EventArgs e)
    {
        var outputPng = Path.GetFullPath(_options.OutPath);
        try
        {
            var directory = Path.GetDirectoryName(outputPng);
            if (!string.IsNullOrEmpty(directory))
            {
                Directory.CreateDirectory(directory);
            }
            var userDataFolder = _options.UserDataDir
                ?? Path.Combine(Path.GetTempPath(), "nai-webview2-smoke");
            Directory.CreateDirectory(userDataFolder);

            var environment = await CoreWebView2Environment.CreateAsync(null, userDataFolder);
            await _webView.EnsureCoreWebView2Async(environment);
            var core = _webView.CoreWebView2 ?? throw new InvalidOperationException("CoreWebView2 was not created");
            core.Settings.AreDevToolsEnabled = false;
            core.Settings.IsStatusBarEnabled = false;

            var navigated = new TaskCompletionSource<CoreWebView2NavigationCompletedEventArgs>(
                TaskCreationOptions.RunContinuationsAsynchronously);
            void OnNavigationCompleted(object? _, CoreWebView2NavigationCompletedEventArgs completed)
                => navigated.TrySetResult(completed);

            core.NavigationCompleted += OnNavigationCompleted;
            try
            {
                core.Navigate(_options.Url);

                var first = await Task.WhenAny(navigated.Task, Task.Delay(NavigationTimeoutMs));
                if (first != navigated.Task)
                {
                    throw new TimeoutException($"navigation did not complete within {NavigationTimeoutMs} ms");
                }
                var completion = await navigated.Task;
                if (!completion.IsSuccess)
                {
                    throw new InvalidOperationException($"navigation failed: {completion.WebErrorStatus}");
                }

                if (_options.WaitMs > 0)
                {
                    await Task.Delay(_options.WaitMs);
                }

                var title = core.DocumentTitle ?? string.Empty;

                using var memory = new MemoryStream();
                await core.CapturePreviewAsync(CoreWebView2CapturePreviewImageFormat.Png, memory);
                memory.Position = 0;

                int width;
                int height;
                using (var bitmap = new Bitmap(memory))
                {
                    width = bitmap.Width;
                    height = bitmap.Height;
                }
                memory.Position = 0;
                await using (var file = File.Create(outputPng))
                {
                    await memory.CopyToAsync(file);
                }

                using var graphics = CreateGraphics();
                Result = new Dictionary<string, object?>
                {
                    ["ok"] = true,
                    ["title"] = title,
                    ["png"] = outputPng,
                    ["pixels"] = new Dictionary<string, object?>
                    {
                        ["width"] = width,
                        ["height"] = height,
                    },
                    ["dpi"] = new Dictionary<string, object?>
                    {
                        ["x"] = graphics.DpiX,
                        ["y"] = graphics.DpiY,
                    },
                    ["url"] = _options.Url,
                    ["bytes"] = new FileInfo(outputPng).Length,
                };
            }
            finally
            {
                core.NavigationCompleted -= OnNavigationCompleted;
            }
        }
        catch (Exception ex)
        {
            Result = new Dictionary<string, object?>
            {
                ["ok"] = false,
                ["title"] = _webView.CoreWebView2?.DocumentTitle,
                ["png"] = outputPng,
                ["error"] = $"{ex.GetType().Name}: {ex.Message}",
            };
        }
        finally
        {
            Close();
        }
    }
}

internal sealed record Options(string Url, string OutPath, int WaitMs, string? UserDataDir)
{
    public static Options Parse(string[] args)
    {
        string? url = null;
        string? output = null;
        string? userDataDir = null;
        var waitMs = 5_000;

        for (var i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--url":
                    url = Value(args, ref i);
                    break;
                case "--out":
                    output = Value(args, ref i);
                    break;
                case "--wait-ms":
                    var raw = Value(args, ref i);
                    if (!int.TryParse(raw, out waitMs) || waitMs < 0 || waitMs > 120_000)
                    {
                        throw new ArgumentException("--wait-ms must be an integer in 0..120000");
                    }
                    break;
                case "--user-data-dir":
                    userDataDir = Value(args, ref i);
                    break;
                default:
                    throw new ArgumentException($"unknown argument: {args[i]}");
            }
        }

        if (string.IsNullOrWhiteSpace(url))
        {
            throw new ArgumentException("--url is required");
        }
        if (!Uri.TryCreate(url, UriKind.Absolute, out var parsed)
            || (parsed.Scheme != Uri.UriSchemeHttp && parsed.Scheme != Uri.UriSchemeHttps))
        {
            throw new ArgumentException("--url must be an absolute http(s) URL");
        }
        if (string.IsNullOrWhiteSpace(output))
        {
            throw new ArgumentException("--out is required");
        }

        return new Options(url, output, waitMs, userDataDir);
    }

    private static string Value(string[] args, ref int index)
    {
        var flag = args[index];
        index++;
        if (index >= args.Length)
        {
            throw new ArgumentException($"missing value for {flag}");
        }
        return args[index];
    }
}
