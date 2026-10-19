using System.IO.Pipes;
using System.Net.Sockets;
using System.Security.Principal;
using Grpc.Net.Client;

namespace Contr.Sdk;

/// <summary>Creates gRPC channels that reach the daemon over its Unix socket or named pipe.</summary>
/// <remarks>
/// Uses the <c>SocketsHttpHandler.ConnectCallback</c> pattern Microsoft documents for gRPC inter-process communication;
/// the <c>http://localhost</c> address only names the HTTP/2 authority.
/// </remarks>
public static class ContrChannel
{
    /// <summary>Channel whose connections all go to <paramref name="endpoint"/>.</summary>
    public static GrpcChannel Create(ContrEndpoint endpoint)
    {
        ArgumentNullException.ThrowIfNull(endpoint);

        var handler = new SocketsHttpHandler
        {
            ConnectCallback = (_, cancellationToken) => ConnectAsync(endpoint, cancellationToken),
        };

        return GrpcChannel.ForAddress("http://localhost", new GrpcChannelOptions
        {
            HttpHandler = handler,
            DisposeHttpClient = true,
        });
    }

    private static ValueTask<Stream> ConnectAsync(ContrEndpoint endpoint, CancellationToken cancellationToken) =>
        endpoint.Kind switch
        {
            ContrEndpointKind.UnixSocket => ConnectSocketAsync(endpoint.Address, cancellationToken),
            _ => ConnectPipeAsync(endpoint.Address, cancellationToken),
        };

    private static async ValueTask<Stream> ConnectSocketAsync(string path, CancellationToken cancellationToken)
    {
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(path), cancellationToken).ConfigureAwait(false);
            return new NetworkStream(socket, ownsSocket: true);
        }
        catch
        {
            socket.Dispose();
            throw;
        }
    }

    private static async ValueTask<Stream> ConnectPipeAsync(string name, CancellationToken cancellationToken)
    {
        var pipe = new NamedPipeClientStream(".", name, PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous);
        try
        {
            await pipe.ConnectAsync(cancellationToken).ConfigureAwait(false);
            return pipe;
        }
        catch
        {
            await pipe.DisposeAsync().ConfigureAwait(false);
            throw;
        }
    }
}
