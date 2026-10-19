using Contr.Sdk;

using var client = await ContrClient.ConnectAsync(clientName: "contr-sample-list-containers");
Console.WriteLine($"Connected to contrd {client.Handshake.DaemonVersion} at {client.Endpoint} (API revision {client.Handshake.ApiRevision})");

try
{
    foreach (var container in await client.ListContainersAsync())
    {
        Console.WriteLine($"{container.Id[..12]}  {container.State,-9}  {string.Join(",", container.Names),-24}  {container.Image}");
    }
}
catch (EngineNotRunningException)
{
    Console.WriteLine("The engine is not running; start it with `contr start`.");
    return 1;
}

return 0;
