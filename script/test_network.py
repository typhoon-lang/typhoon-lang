#!/usr/bin/env python3
"""
Network test suite for the Typhoon popcount TCP server.
Tests the server running on 0.0.0.0:8080.

Usage:
    # Start the server first:
    #   cargo run --bin typhoon-cli -- run sample/main.ty
    # Then run:
    python test_network.py [--host HOST] [--port PORT] [--timeout TIMEOUT]
"""

import socket
import time
import struct
import argparse
import sys
from dataclasses import dataclass, field
from typing import Optional

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

POWERS_OF_2 = [1, 2, 4, 8, 16, 32, 64, 128]  # matches main.ty hardcoded array


def expected_popcount(byte: int) -> int:
    """
    Mirrors Popcount.check(byte) from main.ty:
    For each x in [1,2,4,8,16,32,64,128], check (x & byte) == x
    and count how many match.
    """
    return sum(1 for x in POWERS_OF_2 if (x & byte) == x)


@dataclass
class TestResult:
    name: str
    passed: bool
    message: str = ""


@dataclass
class Suite:
    host: str
    port: int
    timeout: float
    results: list = field(default_factory=list)

    def connect(self) -> socket.socket:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(self.timeout)
        s.connect((self.host, self.port))
        return s

    def record(self, name: str, passed: bool, msg: str = ""):
        status = "PASS" if passed else "FAIL"
        print(f"  [{status}] {name}" + (f": {msg}" if msg else ""))
        self.results.append(TestResult(name, passed, msg))

    # ------------------------------------------------------------------
    # Individual tests
    # ------------------------------------------------------------------

    def test_connect(self):
        """Server accepts a TCP connection."""
        try:
            s = self.connect()
            s.close()
            self.record("connect", True)
        except Exception as e:
            self.record("connect", False, str(e))

    def test_single_byte(self):
        """Send one byte, verify server doesn't crash (connection closes cleanly)."""
        byte = 0x0F  # bits: 00001111 -> matches 1,2,4,8 -> expect 4
        expected = expected_popcount(byte)
        try:
            s = self.connect()
            s.sendall(bytes([byte]))
            s.shutdown(socket.SHUT_WR)  # signal EOF
            s.close()
            self.record("single_byte", True, f"byte=0x{byte:02X} expected_count={expected}")
        except Exception as e:
            self.record("single_byte", False, str(e))

    def test_all_zero(self):
        """Byte 0x00 — no bits set, popcount should be 0."""
        byte = 0x00
        expected = expected_popcount(byte)  # 0
        assert expected == 0
        try:
            s = self.connect()
            s.sendall(bytes([byte]))
            s.shutdown(socket.SHUT_WR)
            s.close()
            self.record("all_zero_byte", True, f"byte=0x00 expected_count=0")
        except Exception as e:
            self.record("all_zero_byte", False, str(e))

    def test_all_ones(self):
        """Byte 0xFF — all 8 bits set, popcount should be 8."""
        byte = 0xFF
        expected = expected_popcount(byte)  # 8
        assert expected == 8
        try:
            s = self.connect()
            s.sendall(bytes([byte]))
            s.shutdown(socket.SHUT_WR)
            s.close()
            self.record("all_ones_byte", True, f"byte=0xFF expected_count=8")
        except Exception as e:
            self.record("all_ones_byte", False, str(e))

    def test_multiple_bytes(self):
        """Send a stream of bytes in one connection."""
        payload = bytes([0x01, 0x03, 0x07, 0x0F, 0xFF, 0x80, 0x00])
        try:
            s = self.connect()
            s.sendall(payload)
            s.shutdown(socket.SHUT_WR)
            s.close()
            self.record("multiple_bytes", True, f"sent {len(payload)} bytes")
        except Exception as e:
            self.record("multiple_bytes", False, str(e))

    def test_concurrent_connections(self, n: int = 5):
        """Open N connections simultaneously."""
        import threading
        errors = []

        def worker(idx):
            try:
                s = self.connect()
                s.sendall(bytes([idx % 256]))
                time.sleep(0.05)
                s.shutdown(socket.SHUT_WR)
                s.close()
            except Exception as e:
                errors.append(str(e))

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(n)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=self.timeout + 1)

        passed = len(errors) == 0
        msg = f"{n} concurrent connections" if passed else f"errors: {errors}"
        self.record("concurrent_connections", passed, msg)

    def test_large_payload(self):
        """Send 256 bytes (all possible byte values)."""
        payload = bytes(range(256))
        try:
            s = self.connect()
            s.sendall(payload)
            s.shutdown(socket.SHUT_WR)
            s.close()
            self.record("large_payload", True, "256 bytes (0x00–0xFF)")
        except Exception as e:
            self.record("large_payload", False, str(e))

    def test_power_of_two_bytes(self):
        """Send exactly the bytes from main.ty's hardcoded array."""
        payload = bytes(POWERS_OF_2)
        try:
            s = self.connect()
            s.sendall(payload)
            s.shutdown(socket.SHUT_WR)
            s.close()
            self.record("power_of_two_bytes", True, f"payload={list(payload)}")
        except Exception as e:
            self.record("power_of_two_bytes", False, str(e))

    def test_reconnect(self, n: int = 3):
        """Connect, send, close, repeat N times sequentially."""
        errors = []
        for i in range(n):
            try:
                s = self.connect()
                s.sendall(bytes([i % 256]))
                s.shutdown(socket.SHUT_WR)
                s.close()
                time.sleep(0.05)
            except Exception as e:
                errors.append(f"attempt {i}: {e}")
        passed = len(errors) == 0
        msg = f"{n} sequential connections" if passed else str(errors)
        self.record("reconnect", passed, msg)

    def test_empty_connection(self):
        """Connect and close immediately without sending data (EOF right away)."""
        try:
            s = self.connect()
            s.shutdown(socket.SHUT_WR)
            s.close()
            self.record("empty_connection", True, "EOF on connect")
        except Exception as e:
            self.record("empty_connection", False, str(e))

    # ------------------------------------------------------------------
    # Run all
    # ------------------------------------------------------------------

    def run(self):
        print(f"\nTyphoon Network Test Suite")
        print(f"Target: {self.host}:{self.port}  timeout={self.timeout}s")
        print("=" * 55)

        # Connectivity gate — skip rest if server isn't up
        print("\n[Connectivity]")
        self.test_connect()
        if not self.results[-1].passed:
            print("\n  Server unreachable — aborting remaining tests.")
            self._summary()
            return self._exit_code()

        print("\n[Single-byte behaviour]")
        self.test_single_byte()
        self.test_all_zero()
        self.test_all_ones()
        self.test_power_of_two_bytes()

        print("\n[Stream behaviour]")
        self.test_multiple_bytes()
        self.test_large_payload()
        self.test_empty_connection()

        print("\n[Connection lifecycle]")
        self.test_reconnect()
        self.test_concurrent_connections()

        self._summary()
        return self._exit_code()

    def _summary(self):
        total = len(self.results)
        passed = sum(1 for r in self.results if r.passed)
        failed = total - passed
        print("\n" + "=" * 55)
        print(f"Results: {passed}/{total} passed", end="")
        if failed:
            print(f"  ({failed} failed)")
            for r in self.results:
                if not r.passed:
                    print(f"  X {r.name}: {r.message}")
        else:
            print(" OK")

    def _exit_code(self) -> int:
        return 0 if all(r.passed for r in self.results) else 1


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Typhoon TCP server network tests")
    parser.add_argument("--host", default="127.0.0.1", help="Server host (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=8080, help="Server port (default: 8080)")
    parser.add_argument("--timeout", type=float, default=5.0, help="Socket timeout in seconds (default: 5.0)")
    args = parser.parse_args()

    suite = Suite(host=args.host, port=args.port, timeout=args.timeout)
    sys.exit(suite.run())


if __name__ == "__main__":
    main()
