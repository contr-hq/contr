using Contr.Sdk;
using Contr.Sdk.Containers.V1;
using Contr.Sdk.Daemon.V1;
using Contr.Sdk.Engine.V1;

var failures = 0;

using var client = await ContrClient.ConnectAsync(clientName: "contr-sdk-verify");

await Check("handshake reports the generated revision", () =>
{
    Expect(client.Handshake.ApiRevision == ContrClient.ApiRevision, $"revision {client.Handshake.ApiRevision}");
    Expect(client.Handshake.BackendKind == BackendKind.Fake, $"backend {client.Handshake.BackendKind}");
    return Task.CompletedTask;
});

await Check("a missing pipe raises DaemonUnavailableException", async () =>
{
    var missing = ContrEndpoint.NamedPipe($"contr-sdk-verify-{Guid.NewGuid():N}");
    await ExpectThrowsAsync<DaemonUnavailableException>(() => ConnectWithinAsync(missing));
});

await Check("a missing socket raises DaemonUnavailableException", async () =>
{
    var missing = ContrEndpoint.UnixSocket(Path.Combine(Path.GetTempPath(), $"contr-sdk-verify-{Guid.NewGuid():N}.sock"));
    await ExpectThrowsAsync<DaemonUnavailableException>(() => ConnectWithinAsync(missing));
});

await Check("a handshake with a cancelled token raises OperationCanceledException", async () =>
{
    await ExpectThrowsAsync<OperationCanceledException>(() => ConnectWithinAsync(client.Endpoint, new CancellationToken(canceled: true)));
});

await Check("cancelling a pending connect raises OperationCanceledException", async () =>
{
    using var cancellation = new CancellationTokenSource(TimeSpan.FromMilliseconds(200));
    var missing = ContrEndpoint.NamedPipe($"contr-sdk-verify-{Guid.NewGuid():N}");
    await ExpectThrowsAsync<OperationCanceledException>(() => ConnectWithinAsync(missing, cancellation.Token));
});

await Check("the engine starts stopped", async () =>
{
    var state = await FirstAsync(ContrClient.ReadAllAsync(client.Engine.WatchState(new WatchStateRequest())));
    Expect(state.State == EngineState.Stopped, $"state {state.State}");
});

await Check("engine calls fail while the engine is stopped", async () =>
{
    await ExpectThrowsAsync<EngineNotRunningException>(() => client.ListContainersAsync());
});

await Check("start streams progress and ends running", async () =>
{
    StartResponse? last = null;
    await foreach (var response in ContrClient.ReadAllAsync(client.Engine.Start(new StartRequest())))
    {
        last = response;
    }

    Expect(last?.State == EngineState.Running, $"last event {last}");
});

await Check("list returns the fixture containers", async () =>
{
    var containers = await client.ListContainersAsync();
    Expect(containers.Count == 3, $"{containers.Count} containers");
});

await Check("info describes the engine", async () =>
{
    var info = await client.Daemon.GetInfoAsync(new GetInfoRequest());
    Expect(info.Engine?.Version == "29.0.0-fake", $"engine {info.Engine}");
});

await Check("watch starts with a snapshot and reports a stop", async () =>
{
    var watch = ContrClient.ReadAllAsync(client.Containers.WatchContainers(new WatchContainersRequest()));
    await using var events = watch.GetAsyncEnumerator();

    Expect(await events.MoveNextAsync(), "no snapshot");
    Expect(events.Current.Snapshot?.Containers.Count == 3, $"first event {events.Current}");

    await client.Containers.StopContainerAsync(new StopContainerRequest { Id = "shop-web-1", TimeoutSeconds = 0 });
    Expect(await events.MoveNextAsync(), "no upsert");
    Expect(events.Current.Upserted?.State == ContainerState.Exited, $"second event {events.Current}");
});

await Check("cancelling a stream read raises OperationCanceledException", async () =>
{
    using var cancellation = new CancellationTokenSource();
    var watch = ContrClient.ReadAllAsync(client.Engine.WatchState(new WatchStateRequest()), cancellation.Token);
    await using var states = watch.GetAsyncEnumerator();

    Expect(await states.MoveNextAsync(), "no state");
    cancellation.CancelAfter(TimeSpan.FromMilliseconds(100));
    await ExpectThrowsAsync<OperationCanceledException>(async () => await states.MoveNextAsync());
});

await Check("inspect returns details and the engine document", async () =>
{
    var inspected = await client.Containers.InspectContainerAsync(new InspectContainerRequest { Id = "shop-db-1" });
    Expect(inspected.Details.Mounts.Single().Name == "shop_pgdata", $"details {inspected.Details}");
    Expect(inspected.RawJson.Contains("\"Name\":\"/shop-db-1\"", StringComparison.Ordinal), "raw JSON lacks the name");
});

await Check("missing containers raise NotFoundException", async () =>
{
    await ExpectThrowsAsync<NotFoundException>(async () =>
    {
        try
        {
            await client.Containers.InspectContainerAsync(new InspectContainerRequest { Id = "missing" });
        }
        catch (Grpc.Core.RpcException exception)
        {
            throw ContrException.From(exception);
        }
    });
});

await Check("removing a running container raises ConflictException", async () =>
{
    await ExpectThrowsAsync<ConflictException>(async () =>
    {
        try
        {
            await client.Containers.RemoveContainerAsync(new RemoveContainerRequest { Id = "shop-db-1" });
        }
        catch (Grpc.Core.RpcException exception)
        {
            throw ContrException.From(exception);
        }
    });
});

await Check("stop ends stopped", async () =>
{
    StopResponse? last = null;
    await foreach (var response in ContrClient.ReadAllAsync(client.Engine.Stop(new StopRequest())))
    {
        last = response;
    }

    Expect(last?.State == EngineState.Stopped, $"last event {last}");
});

Console.WriteLine(failures == 0 ? "Contr.Sdk conformance: all checks passed" : $"Contr.Sdk conformance: {failures} checks failed");
return failures == 0 ? 0 : 1;

async Task Check(string name, Func<Task> body)
{
    try
    {
        await body();
        Console.WriteLine($"ok   {name}");
    }
    catch (Exception exception)
    {
        failures++;
        Console.WriteLine($"FAIL {name}: {exception.Message}");
    }
}

static Task<ContrClient> ConnectWithinAsync(ContrEndpoint endpoint, CancellationToken cancellationToken = default) =>
    ContrClient.ConnectAsync(endpoint, "contr-sdk-verify", cancellationToken).WaitAsync(TimeSpan.FromSeconds(10));

static void Expect(bool condition, string detail)
{
    if (!condition)
    {
        throw new InvalidOperationException(detail);
    }
}

static async Task ExpectThrowsAsync<TException>(Func<Task> body)
    where TException : Exception
{
    try
    {
        await body();
    }
    catch (TException)
    {
        return;
    }

    throw new InvalidOperationException($"expected {typeof(TException).Name}");
}

static async Task<T> FirstAsync<T>(IAsyncEnumerable<T> items)
{
    await foreach (var item in items)
    {
        return item;
    }

    throw new InvalidOperationException("the stream ended without an item");
}
