using System.Net;
using System.Text;
using System.Text.Json;
using NAudio.CoreAudioApi;

namespace VolumeController;

/// <summary>
/// Lightweight HTTP server that receives volume commands and controls
/// Windows system volume via the Core Audio API.
/// </summary>
class Program
{
    private static MMDeviceEnumerator? _enumerator;
    private static MMDevice? _device;
    private static readonly JsonSerializerOptions JsonOpts = new()
    {
        PropertyNameCaseInsensitive = true,
        WriteIndented = true,
    };

    static async Task<int> Main(string[] args)
    {
        int port = 8765;
        if (args.Length > 0 && int.TryParse(args[0], out int parsed))
            port = parsed;

        // Initialize audio
        if (!InitializeAudio())
        {
            Console.Error.WriteLine("Failed to initialize audio device.");
            return 1;
        }

        float currentVol = _device!.AudioEndpointVolume.MasterVolumeLevelScalar;
        Console.WriteLine($"Volume Controller started on port {port}");
        Console.WriteLine($"Current system volume: {currentVol:F2}");
        Console.WriteLine("Waiting for commands... Press Ctrl+C to stop.\n");

        // Start HTTP listener
        var listener = new HttpListener();
        listener.Prefixes.Add($"http://+:{port}/");

        try
        {
            listener.Start();
        }
        catch (HttpListenerException ex) when (ex.ErrorCode == 5)
        {
            Console.Error.WriteLine($"Access denied on port {port}.");
            Console.Error.WriteLine("Run as Administrator, or grant permission with:");
            Console.Error.WriteLine($"  netsh http add urlacl url=http://+:{port}/ user=Everyone");
            return 1;
        }

        Console.CancelKeyPress += (_, e) =>
        {
            e.Cancel = true;
            listener.Stop();
        };

        try
        {
            while (listener.IsListening)
            {
                HttpListenerContext ctx;
                try
                {
                    ctx = await listener.GetContextAsync();
                }
                catch (ObjectDisposedException)
                {
                    break;
                }
                catch (HttpListenerException)
                {
                    break;
                }

                _ = Task.Run(() => HandleRequest(ctx));
            }
        }
        finally
        {
            listener.Close();
            _device?.Dispose();
            _enumerator?.Dispose();
        }

        Console.WriteLine("\nShutdown complete.");
        return 0;
    }

    private static bool InitializeAudio()
    {
        try
        {
            _enumerator = new MMDeviceEnumerator();
            _device = _enumerator.GetDefaultAudioEndpoint(DataFlow.Render, Role.Multimedia);
            return true;
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Audio init error: {ex.Message}");
            return false;
        }
    }

    private static async Task HandleRequest(HttpListenerContext ctx)
    {
        var req = ctx.Request;
        var resp = ctx.Response;
        resp.ContentType = "application/json";

        // CORS headers for convenience
        resp.Headers.Add("Access-Control-Allow-Origin", "*");
        resp.Headers.Add("Access-Control-Allow-Methods", "GET, POST, OPTIONS");
        resp.Headers.Add("Access-Control-Allow-Headers", "Content-Type");

        if (req.HttpMethod == "OPTIONS")
        {
            resp.StatusCode = 204;
            resp.Close();
            return;
        }

        string path = req.Url?.AbsolutePath ?? "/";

        try
        {
            switch (path)
            {
                case "/volume" when req.HttpMethod == "POST":
                    await HandleSetVolume(req, resp);
                    break;
                case "/volume" when req.HttpMethod == "GET":
                    HandleGetVolume(resp);
                    break;
                case "/health":
                    HandleHealth(resp);
                    break;
                default:
                    SendJson(resp, 404, new { error = "Not found" });
                    break;
            }
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"Request error: {ex.Message}");
            SendJson(resp, 500, new { error = "Internal error" });
        }
    }

    private static async Task HandleSetVolume(HttpListenerRequest req, HttpListenerResponse resp)
    {
        if (req.InputStream == null || !req.HasEntityBody)
        {
            SendJson(resp, 400, new { error = "Missing request body" });
            return;
        }

        using var reader = new StreamReader(req.InputStream, Encoding.UTF8);
        string body = await reader.ReadToEndAsync();

        VolumeRequest? volReq;
        try
        {
            volReq = JsonSerializer.Deserialize<VolumeRequest>(body, JsonOpts);
        }
        catch
        {
            SendJson(resp, 400, new { error = "Invalid JSON" });
            return;
        }

        if (volReq == null)
        {
            SendJson(resp, 400, new { error = "Invalid request" });
            return;
        }

        float level = Math.Clamp(volReq.Volume, 0.0f, 1.0f);

        if (_device == null)
        {
            SendJson(resp, 500, new { error = "Audio device not available" });
            return;
        }

        try
        {
            _device.AudioEndpointVolume.MasterVolumeLevelScalar = level;
            Console.WriteLine($"  Volume -> {level:F3}");
            SendJson(resp, 200, new { status = "ok", volume = level });
        }
        catch (Exception ex)
        {
            SendJson(resp, 500, new { error = $"Failed to set volume: {ex.Message}" });
        }
    }

    private static void HandleGetVolume(HttpListenerResponse resp)
    {
        if (_device == null)
        {
            SendJson(resp, 500, new { error = "Audio device not available" });
            return;
        }

        float vol = _device.AudioEndpointVolume.MasterVolumeLevelScalar;
        SendJson(resp, 200, new { volume = vol });
    }

    private static void HandleHealth(HttpListenerResponse resp)
    {
        SendJson(resp, 200, new
        {
            status = "healthy",
            audio_initialized = _device != null,
        });
    }

    private static void SendJson(HttpListenerResponse resp, int statusCode, object data)
    {
        resp.StatusCode = statusCode;
        byte[] buf = JsonSerializer.SerializeToUtf8Bytes(data, JsonOpts);
        resp.ContentLength64 = buf.Length;
        resp.OutputStream.Write(buf, 0, buf.Length);
        resp.Close();
    }
}

record VolumeRequest(float Volume);
