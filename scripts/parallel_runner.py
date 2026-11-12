#!/usr/bin/env python3
"""
Parallel test runner for the external database test suite.
Spawns multiple isolated pgsqlite server instances and distributes
Go database tests across them concurrently.
"""

import argparse
import os
import queue
import re
import socket
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BIN = os.path.join(REPO_ROOT, "target", "release", "pgsqlite")


def resolve_test_dir(user_path=None):
    if user_path:
        return os.path.abspath(user_path)
    if "EXTERNAL_TEST_DIR" in os.environ:
        return os.path.abspath(os.environ["EXTERNAL_TEST_DIR"])
    # Auto-detect test directory in /tmp
    if os.path.exists("/tmp"):
        for entry in sorted(os.listdir("/tmp")):
            cand = os.path.join("/tmp", entry, "db")
            if os.path.isdir(cand):
                try:
                    if any(f.endswith("_test.go") for f in os.listdir(cand)):
                        return os.path.abspath(cand)
                except OSError:
                    pass
    return os.path.abspath("./tests")


def detect_dsn_env_var(test_dir, explicit_var=None):
    if explicit_var:
        return explicit_var
    if "PG_DSN_ENV_VAR" in os.environ:
        return os.environ["PG_DSN_ENV_VAR"]
    # Scan Go source files in the test package / parent directory for DSN var
    search_dirs = [test_dir, os.path.dirname(os.path.abspath(test_dir))]
    for directory in search_dirs:
        if not os.path.exists(directory):
            continue
        for root, _, files in os.walk(directory):
            for f in files:
                if f.endswith(".go"):
                    path = os.path.join(root, f)
                    try:
                        with open(path, "r", encoding="utf-8", errors="ignore") as fp:
                            m = re.search(r"([A-Z0-9_]+_PG_DSN)", fp.read())
                            if m:
                                return m.group(1)
                    except Exception:
                        pass
    return "DATABASE_DSN"


def wait_for_port(port, timeout=10.0):
    start = time.time()
    while time.time() - start < timeout:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(0.5)
                s.connect(("127.0.0.1", port))
                return True
        except Exception:
            time.sleep(0.05)
    return False


def get_tests(test_dir, filter_regex=None):
    cmd = ["go", "test", "-list", filter_regex or ".*", "."]
    out = subprocess.check_output(cmd, cwd=test_dir).decode()
    tests = []
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("Test") and not line.startswith("TestMain"):
            tests.append(line)
    return tests


def parse_args():
    parser = argparse.ArgumentParser(
        description="Parallel runner for external database test suite"
    )
    parser.add_argument(
        "workers_pos",
        nargs="?",
        type=int,
        default=None,
        help="Number of parallel worker instances (positional shorthand)",
    )
    parser.add_argument(
        "-j",
        "--workers",
        type=int,
        default=6,
        help="Number of parallel worker instances (default: 6)",
    )
    parser.add_argument(
        "-d",
        "--test-dir",
        type=str,
        default=None,
        help="Path to external database test suite directory",
    )
    parser.add_argument(
        "-b",
        "--bin",
        type=str,
        default=DEFAULT_BIN,
        help=f"Path to pgsqlite binary (default: {DEFAULT_BIN})",
    )
    parser.add_argument(
        "--base-port",
        type=int,
        default=15500,
        help="Starting port for pgsqlite instances (default: 15500)",
    )
    parser.add_argument(
        "--env-var",
        type=str,
        default=None,
        help="DSN environment variable name expected by tests",
    )
    parser.add_argument(
        "-t",
        "--timeout",
        type=str,
        default="60s",
        help="Per-test timeout passed to go test (default: 60s)",
    )
    parser.add_argument(
        "-r",
        "--run",
        type=str,
        default=None,
        help="Regex filter for specific test names",
    )
    return parser.parse_args()


def main():
    args = parse_args()
    num_workers = args.workers_pos if args.workers_pos is not None else args.workers
    test_dir = resolve_test_dir(args.test_dir)
    pgsqlite_bin = os.path.abspath(args.bin)

    if not os.path.exists(pgsqlite_bin):
        sys.exit(f"Error: pgsqlite binary not found at {pgsqlite_bin}. Run `cargo build --release` first.")
    if not os.path.isdir(test_dir):
        sys.exit(f"Error: test directory not found at {test_dir}.")

    dsn_env_var = detect_dsn_env_var(test_dir, args.env_var)
    tests = get_tests(test_dir, args.run)
    if not tests:
        sys.exit("No tests discovered matching criteria.")

    print(
        f"Discovered {len(tests)} tests. Launching {num_workers} parallel "
        f"pgsqlite instances (DSN variable: {dsn_env_var})..."
    )

    procs = []
    ports = queue.Queue()

    try:
        for i in range(num_workers):
            port = args.base_port + i
            db_path = f"/tmp/parallel_test_{port}.db"
            for ext in ["", "-wal", "-shm"]:
                try:
                    os.unlink(db_path + ext)
                except OSError:
                    pass
            cmd = [pgsqlite_bin, "-p", str(port), "-d", db_path, "--log-level", "error"]
            proc = subprocess.Popen(
                cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
            procs.append((proc, port, db_path))
            if not wait_for_port(port):
                raise RuntimeError(f"pgsqlite failed to start on port {port}")
            ports.put(port)

        print(f"All {num_workers} instances ready! Running tests in parallel...")

        passed = []
        failed = []
        skipped = []
        results_lock = threading.Lock()
        start_time = time.time()

        chunk_size = (len(tests) + num_workers - 1) // num_workers
        worker_chunks = [
            tests[i * chunk_size : (i + 1) * chunk_size] for i in range(num_workers)
        ]

        def run_worker_slice(worker_id):
            port = args.base_port + worker_id
            chunk = worker_chunks[worker_id]
            for test_name in chunk:
                t0 = time.time()
                env = dict(os.environ)
                env[dsn_env_var] = (
                    f"postgres://postgres@127.0.0.1:{port}/test_db?sslmode=disable"
                )
                res = subprocess.run(
                    [
                        "go",
                        "test",
                        "-count=1",
                        "-timeout",
                        args.timeout,
                        "-run",
                        f"^{test_name}$",
                        ".",
                    ],
                    cwd=test_dir,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                )
                elapsed = time.time() - t0
                with results_lock:
                    if res.returncode == 0:
                        if "--- SKIP:" in res.stdout:
                            skipped.append((test_name, elapsed))
                            print(f"  [SKIP] {test_name} ({elapsed:.2f}s)")
                        else:
                            passed.append((test_name, elapsed))
                            print(f"  [PASS] {test_name} ({elapsed:.2f}s)")
                    else:
                        failed.append((test_name, elapsed, res.stdout))
                        print(f"  [FAIL] {test_name} ({elapsed:.2f}s)")

        with ThreadPoolExecutor(max_workers=num_workers) as executor:
            list(executor.map(run_worker_slice, range(num_workers)))

        total_time = time.time() - start_time
        print("\n" + "=" * 60)
        print(f"PARALLEL TEST SUITE SUMMARY (in {total_time:.1f}s)")
        print(
            f"Total: {len(tests)}, Passed: {len(passed)}, "
            f"Failed: {len(failed)}, Skipped: {len(skipped)}"
        )
        print("=" * 60)

        if failed:
            print(f"\n{len(failed)} FAILURES:")
            for name, el, out in failed:
                print(f"\n--- FAIL: {name} ({el:.2f}s) ---")
                for line in out.strip().splitlines()[-10:]:
                    print("  " + line)
            sys.exit(1)
        else:
            print("\nALL TESTS PASSED!")

    finally:
        print("\nTearing down pgsqlite instances...")
        for proc, port, db_path in procs:
            try:
                proc.terminate()
            except OSError:
                pass
        for proc, port, db_path in procs:
            try:
                proc.wait(timeout=2)
            except Exception:
                try:
                    proc.kill()
                except OSError:
                    pass
            for ext in ["", "-wal", "-shm"]:
                try:
                    os.unlink(db_path + ext)
                except OSError:
                    pass
        print("Teardown complete.")


if __name__ == "__main__":
    main()
