"""Benchmark baseline: well-written Python (os.scandir) that answers the same questions as the other baselines.

One walk of <root> computes: file count, total bytes, the 20 largest folders (rolled-up size, the root excluded),
the 20 largest files and the top 10 extensions by size. Results are aggregated while walking, so memory does not
grow with the number of files. Symbolic links and junctions are not followed (the same rule the other baselines use).

  --threads 1   the plain loop most people write: one thread, recursive.
  --threads N   a crawler with N worker threads. os.scandir releases the GIL while it waits for the file system,
                so threads do help; this is the strongest Python the benchmark uses.

Needs Python 3.12 or later (DirEntry.is_junction).
Usage: python -I Overview-PythonScandir.py <root> [--threads N] [--top 20] [--top-ext 10]
"""
import argparse
import heapq
import json
import os
import queue
import sys
import threading

sys.setrecursionlimit(20000)


class Aggregate:
    """The running answers. Methods that change it are called with the lock held (or from one thread)."""

    def __init__(self, top):
        self.top = top
        self.files = 0
        self.ext = {}
        self.big_files = []  # min-heap of (size, path)
        self.big_dirs = []

    def add_folder(self, n_files, ext, big_files):
        self.files += n_files
        for key, size in ext.items():
            self.ext[key] = self.ext.get(key, 0) + size
        for item in big_files:
            self._offer(self.big_files, item)

    def add_dir_total(self, size, path):
        self._offer(self.big_dirs, (size, path))

    def _offer(self, heap, item):
        if len(heap) < self.top:
            heapq.heappush(heap, item)
        elif item > heap[0]:
            heapq.heapreplace(heap, item)


def list_folder(path, top):
    """List one folder (no recursion): sub-folders, file count, bytes, bytes per extension, largest files."""
    subs, n_files, total, ext, files = [], 0, 0, {}, []
    try:
        with os.scandir(path) as entries:
            for e in entries:
                try:
                    if e.is_symlink() or e.is_junction():
                        continue
                    if e.is_dir(follow_symlinks=False):
                        subs.append(e.path)
                        continue
                    size = e.stat(follow_symlinks=False).st_size
                except OSError:
                    continue
                n_files += 1
                total += size
                key = os.path.splitext(e.name)[1].lower()
                ext[key] = ext.get(key, 0) + size
                files.append((size, e.path))
    except OSError:
        pass
    if len(files) > top:
        files = heapq.nlargest(top, files)
    return subs, n_files, total, ext, files


def walk_single(path, agg, is_root):
    subs, n_files, total, ext, files = list_folder(path, agg.top)
    agg.add_folder(n_files, ext, files)
    for sub in subs:
        total += walk_single(sub, agg, False)
    if not is_root:
        agg.add_dir_total(total, path)
    return total


class Node:
    __slots__ = ("path", "parent", "total", "pending")

    def __init__(self, path, parent):
        self.path = path
        self.parent = parent
        self.total = 0
        self.pending = 1  # its own listing; each sub-folder adds one until it finishes


def walk_threads(root, agg, threads):
    work = queue.Queue()
    lock = threading.Lock()
    root_node = Node(root, None)

    def finish(node):
        # lock held: a finished folder adds its total to its parent, which may finish in turn
        while node.pending == 0 and node.parent is not None:
            agg.add_dir_total(node.total, node.path)
            parent = node.parent
            parent.total += node.total
            parent.pending -= 1
            node = parent

    def process(node):
        subs, n_files, total, ext, files = list_folder(node.path, agg.top)
        children = [Node(s, node) for s in subs]
        with lock:
            agg.add_folder(n_files, ext, files)
            node.total += total
            node.pending += len(children) - 1
            finish(node)
        for child in children:
            work.put(child)

    def worker():
        while True:
            node = work.get()
            try:
                if node is None:
                    return
                process(node)
            finally:
                work.task_done()

    pool = [threading.Thread(target=worker, daemon=True) for _ in range(threads)]
    for t in pool:
        t.start()
    work.put(root_node)
    work.join()
    for _ in pool:
        work.put(None)
    for t in pool:
        t.join()
    return root_node.total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("root")
    ap.add_argument("--threads", type=int, default=1)
    ap.add_argument("--top", type=int, default=20)
    ap.add_argument("--top-ext", type=int, default=10)
    args = ap.parse_args()

    root = os.path.abspath(args.root)
    if len(root) > 3:  # keep the backslash of a drive root: "D:" means "the current folder on D:", not its root
        root = root.rstrip("\\")
    agg = Aggregate(args.top)
    total = walk_single(root, agg, True) if args.threads <= 1 else walk_threads(root, agg, args.threads)

    out = {
        "total_files": agg.files,
        "total_bytes": total,
        "top_dirs": [{"path": p, "size": s} for s, p in sorted(agg.big_dirs, reverse=True)],
        "top_files": [{"path": p, "size": s} for s, p in sorted(agg.big_files, reverse=True)],
        "by_ext": [{"ext": k, "size": v} for k, v in sorted(agg.ext.items(), key=lambda kv: -kv[1])[: args.top_ext]],
    }
    json.dump(out, sys.stdout, separators=(",", ":"))


if __name__ == "__main__":
    main()
