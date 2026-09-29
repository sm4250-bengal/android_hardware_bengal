// SPDX-License-Identifier: Apache-2.0
// Same-TU access to the actual internal POSIX adapter/helpers. No production
// test-root option, preprocessor selection, syscall mock, or tracefs access.
#include "../src/posix.cpp"
#include <stdexcept>

using namespace bttrace;
namespace {
void require(bool ok, const char* what) { if (!ok) throw std::runtime_error(what); }
}
int main(int argc, char** argv) {
    try {
        require(argc == 2, "existing isolated fixture directory required");
        int root = open(argv[1], O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        require(root >= 0, "fixture directory");
        IoError e;
        require(beneath(root, "absent/child", O_RDONLY, e) < 0 &&
                e.operation == "openat" && e.number == ENOENT && e.detail == "absent", "real openat ENOENT");
        close(-1); // force a different ambient errno after the failed open/close sequence
        require(e.number == ENOENT, "close does not overwrite captured open error");
        e = {};
        require(beneath(-1, "child", O_RDONLY, e) < 0 && e.operation == "fcntl-dup" && e.number == EBADF,
                "real fcntl EBADF");
        e = {};
        require(beneath(root, "../escape", O_RDONLY, e) < 0 && e.operation == "path-check" && !e.number,
                "path refusal does not invent errno");
        require(symlinkat("/dev/null", root, "link") == 0, "symlink fixture");
        e = {};
        require(beneath(root, "link", O_RDONLY, e) < 0 && e.operation == "openat" && e.number == ELOOP,
                "real nofollow ELOOP");

        int full = open("/dev/full", O_WRONLY | O_CLOEXEC);
        require(full >= 0, "full fixture");
        e = {};
        require(!write_once(full, "256\n", e) && e.operation == "write" && e.number == ENOSPC,
                "actual control-write helper ENOSPC");
        close(full); close(-1);
        require(e.number == ENOSPC, "write errno survives closes");
        e = {};
        require(!write_once(-1, "x", e) && e.operation == "write" && e.number == EBADF, "real write EBADF");

        int pipes[2]; require(pipe2(pipes, O_NONBLOCK | O_CLOEXEC) == 0, "nonblocking pipe fixture");
        int capacity = fcntl(pipes[1], F_GETPIPE_SZ);
        require(capacity > 0 && capacity <= 1024 * 1024, "bounded pipe capacity");
        e = {}; errno = EACCES;
        require(!write_once(pipes[1], std::string(size_t(capacity) * 2, 'x'), e) &&
                e.operation == "write-short" && !e.number && !e.detail.empty(),
                "real partial write carries observed count, never stale errno");
        close(pipes[0]); close(pipes[1]);

        PosixTrace io; // deliberately unopened; no production root is accessed
        Read read = io.read("tracing_on", 32, Clock().now() + kSecond);
        require(!read.ok && read.error.operation == "fcntl-dup" && read.error.number == EBADF,
                "actual read adapter captures its failing syscall");
        require(!io.write("tracing_on", "0\n") && io.error().number == EBADF, "actual write adapter error");
        require(!io.create("owned") && io.error().operation == "mkdirat" && io.error().number == EBADF,
                "actual mkdirat error");
        require(!io.remove("owned") && io.error().operation == "unlinkat" && io.error().number == EBADF,
                "actual unlinkat error");
        require(io.cpus().empty() && io.error().operation == "fcntl-dup" && io.error().number == EBADF,
                "actual CPU enumeration error");
        Read expired = io.read("tracing_on", 32, 0);
        require(!expired.ok && expired.error.operation == "read-budget-check" && !expired.error.number,
                "expired read does not invent syscall errno");
        close(root);
        puts("PASS real POSIX error snapshots: openat/dup/write/mkdirat/unlinkat/CPU/read, short write and refusals");
        return 0;
    } catch (const std::exception& ex) { fprintf(stderr, "FAIL %s\n", ex.what()); return 1; }
}
