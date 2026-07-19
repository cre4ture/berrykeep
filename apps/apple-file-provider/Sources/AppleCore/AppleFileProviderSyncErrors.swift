@preconcurrency import FileProvider
import Foundation

public func ironmeshConstraintError(_ reason: String) -> NSError {
    NSError(
        domain: NSFileProviderErrorDomain,
        code: NSFileProviderError.Code.serverUnreachable.rawValue,
        userInfo: [NSLocalizedDescriptionKey: reason]
    )
}

public func ironmeshConflictError(
    originalPath: String,
    conflictCopyPath: String,
    expectedRevision: String,
    currentRevision: String
) -> NSError {
    NSError(
        domain: NSFileProviderErrorDomain,
        code: NSFileProviderError.Code.cannotSynchronize.rawValue,
        userInfo: [
            NSLocalizedDescriptionKey:
                "The remote version of \(originalPath) changed. Your edit was preserved as \(conflictCopyPath).",
            "IronmeshConflictCopyPath": conflictCopyPath,
            "IronmeshExpectedRevision": expectedRevision,
            "IronmeshCurrentRevision": currentRevision,
        ]
    )
}

public func ironmeshRevisionConflictError(
    path: String,
    expectedRevision: String,
    currentRevision: String
) -> NSError {
    NSError(
        domain: NSFileProviderErrorDomain,
        code: NSFileProviderError.Code.cannotSynchronize.rawValue,
        userInfo: [
            NSLocalizedDescriptionKey:
                "The remote version of \(path) changed. Refresh the item before retrying this operation.",
            "IronmeshExpectedRevision": expectedRevision,
            "IronmeshCurrentRevision": currentRevision,
        ]
    )
}
