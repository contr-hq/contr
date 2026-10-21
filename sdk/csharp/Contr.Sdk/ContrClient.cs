using System.Diagnostics;
using System.Runtime.CompilerServices;
using Contr.Sdk.Compose.V1;
using Contr.Sdk.Containers.V1;
using Contr.Sdk.Daemon.V1;
using Contr.Sdk.Diagnostics.V1;
using Contr.Sdk.Engine.V1;
using Contr.Sdk.Images.V1;
using Contr.Sdk.Integration.V1;
using Contr.Sdk.Kubernetes.V1;
using Contr.Sdk.Networks.V1;
using Contr.Sdk.Settings.V1;
using Contr.Sdk.Updates.V1;
using Contr.Sdk.Volumes.V1;
using Grpc.Core;
using Grpc.Net.Client;

namespace Contr.Sdk;

/// <summary>Connection to a daemon that completed the handshake; dispose it to close the connection.</summary>
public sealed class ContrClient : IDisposable
{
    private static readonly TimeSpan StartTimeout = TimeSpan.FromSeconds(15);
    private static readonly TimeSpan StartPollInterval = TimeSpan.FromMilliseconds(100);

    private readonly GrpcChannel channel;

    private ContrClient(GrpcChannel channel, ContrEndpoint endpoint, HandshakeResponse handshake)
    {
        this.channel = channel;
        Endpoint = endpoint;
        Handshake = handshake;
        Daemon = new DaemonService.DaemonServiceClient(channel);
        Engine = new EngineService.EngineServiceClient(channel);
        Containers = new ContainersService.ContainersServiceClient(channel);
        Images = new ImagesService.ImagesServiceClient(channel);
        Volumes = new VolumesService.VolumesServiceClient(channel);
        Networks = new NetworksService.NetworksServiceClient(channel);
        Compose = new ComposeService.ComposeServiceClient(channel);
        Kubernetes = new KubernetesService.KubernetesServiceClient(channel);
        Integration = new IntegrationService.IntegrationServiceClient(channel);
        Diagnostics = new DiagnosticsService.DiagnosticsServiceClient(channel);
        Settings = new SettingsService.SettingsServiceClient(channel);
        Updates = new UpdatesService.UpdatesServiceClient(channel);
    }

    /// <summary>Value of <c>proto/API_REVISION</c> this SDK was generated from.</summary>
    public static uint ApiRevision => Product.ApiRevision;

    /// <summary>Endpoint the client is connected to.</summary>
    public ContrEndpoint Endpoint { get; }

    /// <summary>The daemon's handshake answer, including its API revision and capabilities.</summary>
    public HandshakeResponse Handshake { get; }

    /// <summary>Daemon identity service.</summary>
    public DaemonService.DaemonServiceClient Daemon { get; }

    /// <summary>Engine lifecycle service.</summary>
    public EngineService.EngineServiceClient Engine { get; }

    /// <summary>Containers service.</summary>
    public ContainersService.ContainersServiceClient Containers { get; }

    /// <summary>Images service.</summary>
    public ImagesService.ImagesServiceClient Images { get; }

    /// <summary>Volumes service.</summary>
    public VolumesService.VolumesServiceClient Volumes { get; }

    /// <summary>Networks service.</summary>
    public NetworksService.NetworksServiceClient Networks { get; }

    /// <summary>Compose projects service.</summary>
    public ComposeService.ComposeServiceClient Compose { get; }

    /// <summary>Kubernetes cluster service.</summary>
    public KubernetesService.KubernetesServiceClient Kubernetes { get; }

    /// <summary>Integration with the user's tools: Docker CLI tools and WSL distributions.</summary>
    public IntegrationService.IntegrationServiceClient Integration { get; }

    /// <summary>Checks of what keeps the engine from working, and the fixes the daemon runs.</summary>
    public DiagnosticsService.DiagnosticsServiceClient Diagnostics { get; }

    /// <summary>The user's settings and the ones an administrator fixes.</summary>
    public SettingsService.SettingsServiceClient Settings { get; }

    /// <summary>Whether a newer Contr is published.</summary>
    public UpdatesService.UpdatesServiceClient Updates { get; }

    /// <summary>Connects to <paramref name="endpoint"/>, or to <see cref="ContrEndpoint.Resolve"/>, and performs the handshake.</summary>
    public static async Task<ContrClient> ConnectAsync(
        ContrEndpoint? endpoint = null,
        string clientName = "contr-sdk-dotnet",
        CancellationToken cancellationToken = default)
    {
        endpoint ??= ContrEndpoint.Resolve();
        var channel = ContrChannel.Create(endpoint);
        var request = new HandshakeRequest
        {
            ClientName = clientName,
            ClientVersion = Product.SdkVersion,
            ApiRevision = Product.ApiRevision,
        };

        try
        {
            var handshake = await new DaemonService.DaemonServiceClient(channel)
                .HandshakeAsync(request, cancellationToken: cancellationToken)
                .ConfigureAwait(false);
            return new ContrClient(channel, endpoint, handshake);
        }
        catch (RpcException exception)
        {
            channel.Dispose();
            throw exception.StatusCode == StatusCode.Unimplemented
                ? new VersionMismatchException($"The server at {endpoint} is not a {Product.DisplayName} daemon.", exception)
                : Classify(exception, cancellationToken);
        }
    }

    /// <summary>Connects, first running the daemon executable when nothing answers and <c>CONTR_HOST</c> is unset.</summary>
    public static async Task<ContrClient> ConnectOrStartAsync(
        string clientName = "contr-sdk-dotnet",
        CancellationToken cancellationToken = default)
    {
        try
        {
            return await ConnectAsync(null, clientName, cancellationToken).ConfigureAwait(false);
        }
        catch (DaemonUnavailableException) when (Environment.GetEnvironmentVariable(Product.HostEnv) is null)
        {
            StartDaemon();
        }

        var deadline = DateTime.UtcNow + StartTimeout;
        while (true)
        {
            try
            {
                return await ConnectAsync(null, clientName, cancellationToken).ConfigureAwait(false);
            }
            catch (DaemonUnavailableException) when (DateTime.UtcNow < deadline)
            {
                await Task.Delay(StartPollInterval, cancellationToken).ConfigureAwait(false);
            }
        }
    }

    /// <summary>Every container, stopped ones included.</summary>
    public async Task<IReadOnlyList<Container>> ListContainersAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            var response = await Containers
                .ListContainersAsync(new ListContainersRequest(), cancellationToken: cancellationToken)
                .ConfigureAwait(false);
            return response.Containers;
        }
        catch (RpcException exception)
        {
            throw Classify(exception, cancellationToken);
        }
    }

    /// <summary>Reads a server stream, turning gRPC failures into <see cref="ContrException"/> subclasses.</summary>
    public static async IAsyncEnumerable<T> ReadAllAsync<T>(
        AsyncServerStreamingCall<T> call,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(call);

        using (call)
        {
            var stream = call.ResponseStream;
            while (true)
            {
                try
                {
                    if (!await stream.MoveNext(cancellationToken).ConfigureAwait(false))
                    {
                        yield break;
                    }
                }
                catch (RpcException exception)
                {
                    throw Classify(exception, cancellationToken);
                }

                yield return stream.Current;
            }
        }
    }

    /// <inheritdoc/>
    public void Dispose() => channel.Dispose();

    private static Exception Classify(RpcException exception, CancellationToken cancellationToken) =>
        exception.StatusCode == StatusCode.Cancelled && cancellationToken.IsCancellationRequested
            ? new OperationCanceledException(exception.Status.Detail, exception, cancellationToken)
            : ContrException.From(exception);

    private static void StartDaemon()
    {
        var executable = Product.DaemonBinary + (OperatingSystem.IsWindows() ? ".exe" : string.Empty);
        var startInfo = new ProcessStartInfo(executable)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
        };

        try
        {
            using var process = Process.Start(startInfo);
        }
        catch (System.ComponentModel.Win32Exception exception)
        {
            throw new DaemonUnavailableException($"Cannot run {executable}: {exception.Message}", exception);
        }
    }
}
