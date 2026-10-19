using System.Text.Json;

namespace Contr.Sdk;

/// <summary>Kind of local transport an endpoint uses.</summary>
public enum ContrEndpointKind
{
    /// <summary>Unix domain socket.</summary>
    UnixSocket,

    /// <summary>Windows named pipe.</summary>
    NamedPipe,
}

/// <summary>Address of the daemon's gRPC API, written <c>unix:///PATH</c> or <c>npipe:////./pipe/NAME</c>.</summary>
public sealed record ContrEndpoint
{
    private const string UnixScheme = "unix://";
    private const string PipeScheme = "npipe://";
    private const string PipePrefix = @"\\.\pipe\";

    private ContrEndpoint(ContrEndpointKind kind, string address)
    {
        Kind = kind;
        Address = address;
    }

    /// <summary>Transport the endpoint uses.</summary>
    public ContrEndpointKind Kind { get; }

    /// <summary>Absolute socket path, or the pipe name without the <c>\\.\pipe\</c> prefix.</summary>
    public string Address { get; }

    /// <summary>Endpoint of a Unix domain socket at an absolute path.</summary>
    public static ContrEndpoint UnixSocket(string path) =>
        Path.IsPathRooted(path)
            ? new ContrEndpoint(ContrEndpointKind.UnixSocket, path)
            : throw new ArgumentException("The socket path must be absolute.", nameof(path));

    /// <summary>Endpoint of a named pipe on this machine.</summary>
    public static ContrEndpoint NamedPipe(string name) =>
        new(ContrEndpointKind.NamedPipe, name.StartsWith(PipePrefix, StringComparison.Ordinal) ? name[PipePrefix.Length..] : name);

    /// <summary>Parses the <c>unix://</c> and <c>npipe://</c> forms the daemon and <c>CONTR_HOST</c> use.</summary>
    public static ContrEndpoint Parse(string text) =>
        TryParse(text, out var endpoint) ? endpoint : throw new FormatException($"Invalid endpoint '{text}'; expected unix://PATH or npipe://PATH.");

    /// <summary>Parses an endpoint string; returns <see langword="false"/> for any other form.</summary>
    public static bool TryParse(string text, [System.Diagnostics.CodeAnalysis.NotNullWhen(true)] out ContrEndpoint? endpoint)
    {
        endpoint = null;
        if (text.StartsWith(UnixScheme, StringComparison.Ordinal))
        {
            var path = text[UnixScheme.Length..];
            endpoint = path.StartsWith('/') ? new ContrEndpoint(ContrEndpointKind.UnixSocket, path) : null;
        }
        else if (text.StartsWith(PipeScheme, StringComparison.Ordinal))
        {
            var name = text[PipeScheme.Length..].TrimStart('/').Replace('/', '\\');
            const string local = @".\pipe\";
            endpoint = name.StartsWith(local, StringComparison.Ordinal) && name.Length > local.Length
                ? new ContrEndpoint(ContrEndpointKind.NamedPipe, name[local.Length..])
                : null;
        }

        return endpoint is not null;
    }

    /// <summary>Endpoint from <c>CONTR_HOST</c>, else from the running daemon's discovery file, else the per-user default.</summary>
    public static ContrEndpoint Resolve()
    {
        var host = Environment.GetEnvironmentVariable(Product.HostEnv);
        if (!string.IsNullOrEmpty(host))
        {
            return Parse(host);
        }

        return Discovered() ?? DefaultForUser();
    }

    /// <summary>Endpoint the daemon serves when started without one.</summary>
    public static ContrEndpoint DefaultForUser()
    {
        if (OperatingSystem.IsWindows())
        {
            var sid = System.Security.Principal.WindowsIdentity.GetCurrent().User?.Value
                ?? throw new InvalidOperationException("The current Windows user has no SID.");
            return NamedPipe($"{Product.Slug}-{sid}-api");
        }

        var runtimeDir = RuntimeDirectory() ?? throw new InvalidOperationException("XDG_RUNTIME_DIR is not set.");
        return UnixSocket(Path.Combine(runtimeDir, Product.ApiSocketFile));
    }

    /// <inheritdoc/>
    public override string ToString() => Kind switch
    {
        ContrEndpointKind.UnixSocket => UnixScheme + Address,
        _ => $"{PipeScheme}//./pipe/{Address}",
    };

    private static ContrEndpoint? Discovered()
    {
        var runtimeDir = RuntimeDirectory();
        var file = runtimeDir is null ? null : Path.Combine(runtimeDir, Product.DiscoveryFile);
        if (file is null || !File.Exists(file))
        {
            return null;
        }

        try
        {
            using var document = JsonDocument.Parse(File.ReadAllText(file));
            foreach (var element in document.RootElement.GetProperty("endpoints").EnumerateArray())
            {
                if (element.GetString() is { } text && TryParse(text, out var endpoint))
                {
                    return endpoint;
                }
            }
        }
        catch (Exception exception) when (exception is IOException or JsonException or KeyNotFoundException or InvalidOperationException)
        {
            return null;
        }

        return null;
    }

    private static string? RuntimeDirectory()
    {
        if (OperatingSystem.IsWindows())
        {
            var local = Environment.GetEnvironmentVariable("LOCALAPPDATA");
            return string.IsNullOrEmpty(local) ? null : Path.Combine(local, Product.DisplayName, "run");
        }

        var runtime = Environment.GetEnvironmentVariable("XDG_RUNTIME_DIR");
        return string.IsNullOrEmpty(runtime) ? null : Path.Combine(runtime, Product.Slug);
    }
}
