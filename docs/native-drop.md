# Native drop: the drag stays a drag on the other side

Status: the Mac receiver is implemented (0.1.1). The Windows receiver and dragging out of Windows are designed and
spiked (2026-11-05), not implemented; on Windows an arriving drag still lands in Downloads.

## Goal

Files dragged across an edge are still being dragged on the machine they arrive on. Releasing over a folder puts
them in that folder; releasing over an app that takes files hands them to it. Today they always land in
Downloads.

## What the spikes proved

Both ran on the receiving machine with input injected the way a driven Flowseam injects it (posted mouse events;
on the Mac with the mouse disassociated from the cursor).

| | macOS (Finder) | Windows (Explorer) |
|---|---|---|
| Start a system drag when the button is down only through injected input | yes: a transparent window under the cursor gets the posted `leftMouseDown`, `beginDraggingSession` starts from it | yes: a window under the cursor gets the `SendInput` down, `DoDragDrop` enters its loop |
| Injected moves carry it | yes, also disassociated | yes |
| Drop on a folder writes a file that does not exist yet | yes: `NSFilePromiseProvider`, Finder calls `writePromiseTo:` with the folder | yes: `FileGroupDescriptorW` + `FileContents`, Explorer reads the contents |
| Cancel from our side | Escape posted to our own pid | `QueryContinueDrag` returns cancel |
| Released while the bytes are still arriving (6 s late) | the session ends at once; the promise callback blocks on its own queue and Finder waits for it; the file lands complete | Explorer reads `FileContents` on another thread and waits; its window is never reported hung; the 200 KB file lands complete. `DoDragDrop` returns only when the read is done, so it needs a thread of its own |
| Runs | 10 drops (1 unexplained failure, the first), 1 cancel, 1 late | 5 drops, 1 cancel, 1 late (own COM `IDataObject` and `IStream`; .NET's wrapper never reads a custom stream) |

Dragging out of Windows (the reverse): a topmost OLE drop target along the edge gets `DragEnter` when an Explorer drag
reaches it, reads the real paths from `FileDrop`, refuses the drop, and Escape ends Explorer's drag with the file
left where it was (1 clean run; the harness's UI Automation lookup of the item failed on the other run).

## Design

- **Receiver** (`DragPump` / `Incoming`): `DragSoon` or `DragBegin` starts a native session through a new sink
  hook; the session owns the drop. A drop on a target arrives as a hub job with the destination folder; the files
  are moved there as each finishes (the callback waits per file). A session that ends with no target falls back to
  today's drop into Downloads. Carrying the drag back, or the sender's cancel, ends the session (Escape / cancel
  flag) and deletes the staged files.
- **Threads**: the promise and stream callbacks run on AppKit and OLE threads. They never lock a pump: they get a
  per-file completion handle from the pump and report back through the hub's queue, like every other drag event.
- **Forwarder**: the receiver presses the primary button itself to start the session. That press is tracked apart
  from the peer's presses: released on every way out (Leave, link down, emergency) so the button never sticks, and
  not counted as "held here", so the drag can still be carried back out.
- **Protocol**: none. `DragBegin` already carries names and sizes, so mixed versions keep working.
- **Mac**: the worker's accessory `NSApplication` (front.rs) hosts the drag window; two Objective-C classes for the
  dragging source and the promise delegate.
- **Windows**: a dedicated STA thread with `IDataObject`, `IDropSource`, `IEnumFORMATETC` and `IStream`; it blocks in
  `DoDragDrop` until the target has read the files, which must not hold up input.
- **Windows as the source**: a thin topmost drop-target window along each edge with a peer; `DragEnter` with files
  is the outgoing drag (the same role the drag pasteboard plays on the Mac), then Escape ends the local drag.

## Order

1. Mac receiver (mini → mbp), end to end, with the shared pump and forwarder changes.
2. Windows receiver (mini → win).
3. Dragging from Windows (the edge drop target above).

## Known ceiling

Finder, Explorer and other apps that accept promised or virtual files can take a drop while bytes are still in
transit. Apps that take only plain file paths cannot; adding plain paths once a transfer completes is a later step.
