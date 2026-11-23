import Foundation

final class FolderWatcher {
    private let source: DispatchSourceFileSystemObject

    init?(folder: URL, onChange: @escaping () -> Void) {
        let fd = open(folder.path, O_EVTONLY | O_CLOEXEC)
        guard fd >= 0 else { return nil }
        source = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: fd,
            eventMask: [.write, .rename, .delete, .revoke],
            queue: .main
        )
        source.setEventHandler(handler: onChange)
        source.setCancelHandler { close(fd) }
        source.resume()
    }

    deinit { source.cancel() }
}
