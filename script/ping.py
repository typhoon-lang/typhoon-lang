import os
import socket
import sys
import time

host, port, max_wait = "127.0.0.1", 8080, 120
print(f"Waiting up to {max_wait}s for {host}:{port} ...")

for i in range(max_wait):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(2)
        s.connect((host, port))
        # Send a harmless probe byte and wait briefly to confirm
        # accept() has run on the server side
        s.sendall(b"\x00")
        try:
            s.recv(1)
        except OSError:
            pass
        s.close()
        print(f"  -> server accepted connection after {i + 1}s")
        sys.exit(0)
    except OSError as e:
        print(f"  [{i + 1:2d}s] not ready ({e})")
        time.sleep(1)

for log in ("server.log", "server-err.log"):
    if os.path.exists(log) and os.path.getsize(log) > 0:
        print(f"\n--- {log} ---")
        with open(log) as f:
            print(f.read())

print("ERROR: server did not start in time")
sys.exit(1)
