# 0010. Long-running work runs as operations the daemon owns

## Context

Several requests take seconds to minutes: pulling and pushing images, Compose actions, importing images into the
cluster, writing a diagnostics bundle, running a fix, and starting, stopping, restarting or resetting the engine. Each is
a server-streaming RPC whose stream carries the progress.

- Pulls, pushes, Compose actions and image imports end with their stream. `dockerd` cancels a pull or push whose HTTP
  client disconnects, the daemon kills `docker compose` when the stream is dropped, and the import is a lazy stream.
  Quitting a client therefore cancels the work it started.
- Engine operations and fixes already run on the daemon's own tasks, but their steps reach only the caller. A bundle
  is written on a daemon task, and its zip reaches only the caller.
- Both clients keep an operation's progress in the page that started it. Leaving that page, or restarting the client,
  loses the progress, and nothing stops the user starting the same action twice.
- Several clients watch one daemon: the Windows or Linux app, the CLI, and SDK users. Each should see work any of them
  started.

## Decision

- The daemon runs this work as operations: each runs on a daemon task, independent of any RPC, until it finishes,
  fails, is cancelled, or the daemon stops.
- `contr.operations.v1.OperationsService`:
  - `StartOperation` takes one of the existing request messages (`PullImageRequest`, `RunActionRequest`, and so on),
    validates it as the existing RPC does, starts the operation and returns it.
  - `WatchOperations` streams every operation's summary: a snapshot, then upserts and removals, as the other watches do.
  - `FollowOperation` replays what the daemon kept of one operation's events, then streams new ones until it ends.
    Events are the existing response messages, so a client renders them as it renders the existing RPCs.
  - `CancelOperation` cancels one. A cancelled pull or push stops downloading or uploading, and layers already stored
    stay. A cancelled Compose action stops `docker compose` as Ctrl+C would. A cancelled import stops before its next
    image. A cancelled bundle is discarded. Engine operations and fixes cannot be cancelled, because they would leave
    the engine or the computer half changed.
- An operation records its kind, its request, its state (running, succeeded, failed or cancelled), its latest step,
  when it started and finished, and why it failed.
- Starting work that is already running returns the running operation for:
  - a pull of the same reference and platform;
  - a push of the same reference;
  - a bundle;
  - a fix with the same action;
  - an engine operation of the same kind and scope.

  Other engine operations queue behind the running one, as the engine RPCs do. A Compose action on a project with a
  running action fails with FAILED_PRECONDITION.
- Replay keeps:
  - the latest progress of each pull layer and the latest push status;
  - the last 1,000 lines of Compose output;
  - every import result and every fix or engine step;
  - a bundle's zip, until the operation is removed.
- A finished operation stays listed for an hour, and at most the 50 most recent are kept. Operations exist only in the
  daemon's memory, and a daemon that stops cancels those still running.
- The existing streaming RPCs keep their contract (rule 1 of [COMPATIBILITY.md](../../COMPATIBILITY.md)). They run
  their work as an operation, so other clients see it. Dropping a pull, push, Compose action, import or bundle stream
  still cancels the operation, which the CLI relies on for Ctrl+C. The engine RPCs and `RunRemediation` keep running
  as before.
- Kubernetes enable and disable are not operations: the cluster's own state and steps in `WatchCluster` already
  outlive every client.

## Consequences

- A client can leave a page, close its window or quit, and the work goes on. Any client shows every running
  operation, from any client, and offers Cancel where the operation allows it.
- Clients drop their per-page progress state. One activity view follows `WatchOperations`, and a page follows the
  operation for its own subject, such as a project's Compose action.
- The daemon holds a bounded amount of state per operation, and up to 50 bundles' zips in the worst case.
- A client that wants an operation tied to its own lifetime uses the existing RPC, or cancels the operation itself.
- Clients built before this revision keep working unchanged.
