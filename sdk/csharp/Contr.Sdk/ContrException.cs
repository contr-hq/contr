using Contr.Sdk.Common.V1;
using Google.Protobuf.Reflection;
using Google.Rpc;
using Grpc.Core;

namespace Contr.Sdk;

/// <summary>Failure of a call to the daemon, classified from its gRPC status and <c>google.rpc.ErrorInfo</c> reason.</summary>
public class ContrException : Exception
{
    /// <summary>Creates an exception for a classified failure.</summary>
    public ContrException(string message, ErrorReason reason, IReadOnlyList<Remediation> remediations, Exception? inner)
        : base(message, inner)
    {
        Reason = reason;
        Remediations = remediations;
    }

    /// <summary>Reason the daemon attached; <see cref="ErrorReason.Unspecified"/> when it attached none.</summary>
    public ErrorReason Reason { get; }

    /// <summary>Actions that resolve the failure, in the order clients should offer them.</summary>
    public IReadOnlyList<Remediation> Remediations { get; }

    /// <summary>Classifies a failed call.</summary>
    public static ContrException From(RpcException exception)
    {
        ArgumentNullException.ThrowIfNull(exception);

        var status = exception.GetRpcStatus();
        var reason = ParseReason(status?.GetDetail<ErrorInfo>()?.Reason);
        var remediations = status?.Details
            .Where(detail => detail.Is(Remediation.Descriptor))
            .Select(detail => detail.Unpack<Remediation>())
            .ToArray() ?? [];
        var message = exception.Status.Detail;

        return (reason, exception.StatusCode) switch
        {
            (ErrorReason.EngineNotRunning, _) => new EngineNotRunningException(message, exception),
            (ErrorReason.FeatureUnavailable, _) or (ErrorReason.Unspecified, StatusCode.Unimplemented) =>
                new FeatureUnavailableException(message, reason, exception),
            (ErrorReason.SettingLocked, _) => new SettingLockedException(message, exception),
            (ErrorReason.SocketPermissionDenied, _) or (ErrorReason.Unspecified, StatusCode.PermissionDenied) =>
                new PermissionDeniedException(message, reason, remediations, exception),
            (ErrorReason.Unspecified, StatusCode.NotFound) => new NotFoundException(message, exception),
            (ErrorReason.Unspecified, StatusCode.FailedPrecondition or StatusCode.AlreadyExists or StatusCode.Aborted) =>
                new ConflictException(message, exception),
            (ErrorReason.Unspecified, StatusCode.Unavailable) => new DaemonUnavailableException(message, exception),
            (ErrorReason.Unspecified, StatusCode.Cancelled) => new DismissedException(message, exception),
            _ => new ContrException(message, reason, remediations, exception),
        };
    }

    private static ErrorReason ParseReason(string? name) =>
        name is not null
            && ErrorsReflection.Descriptor.FindTypeByName<EnumDescriptor>(nameof(ErrorReason))?.FindValueByName($"ERROR_REASON_{name}") is { } value
            ? (ErrorReason)value.Number
            : ErrorReason.Unspecified;
}

/// <summary>The engine is stopped or still starting.</summary>
public sealed class EngineNotRunningException(string message, Exception? inner)
    : ContrException(message, ErrorReason.EngineNotRunning, [], inner);

/// <summary>The object does not exist.</summary>
public sealed class NotFoundException(string message, Exception? inner)
    : ContrException(message, ErrorReason.Unspecified, [], inner);

/// <summary>The object already exists, or its state forbids the request.</summary>
public sealed class ConflictException(string message, Exception? inner)
    : ContrException(message, ErrorReason.Unspecified, [], inner);

/// <summary>The user dismissed a prompt the operation needs, so nothing changed.</summary>
public sealed class DismissedException(string message, Exception? inner)
    : ContrException(message, ErrorReason.Unspecified, [], inner);

/// <summary>The daemon or its backend does not support the request; hide the feature.</summary>
public sealed class FeatureUnavailableException(string message, ErrorReason reason, Exception? inner)
    : ContrException(message, reason, [], inner);

/// <summary>Access was refused; <see cref="ContrException.Remediations"/> lists the ways to gain it.</summary>
public sealed class PermissionDeniedException(string message, ErrorReason reason, IReadOnlyList<Remediation> remediations, Exception? inner)
    : ContrException(message, reason, remediations, inner);

/// <summary>An administrator policy fixes the setting.</summary>
public sealed class SettingLockedException(string message, Exception? inner)
    : ContrException(message, ErrorReason.SettingLocked, [], inner);

/// <summary>The server is not a Contr daemon this SDK can talk to.</summary>
public sealed class VersionMismatchException(string message, Exception? inner)
    : ContrException(message, ErrorReason.Unspecified, [], inner);

/// <summary>No daemon answered at the endpoint.</summary>
public sealed class DaemonUnavailableException(string message, Exception? inner)
    : ContrException(message, ErrorReason.Unspecified, [], inner);
