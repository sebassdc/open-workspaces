#!/usr/bin/env python3
"""Actual one-guest hardware acceptance, run against a dedicated fixture root."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import time

class Guest:
    MATCH_LIMIT = 256 * 1024
    LOG_LIMIT = 64 * 1024
    READY_TIMEOUT = 30
    STOP_TIMEOUT = 10
    TERMINATE_TIMEOUT = 5
    KILL_TIMEOUT = 5

    def __init__(self, helper, root, disk="persist.img", mode="boot", via_cli=False):
        self.start = time.monotonic()
        argv = [str(helper), mode, str(root/"Image"), str(root/"initramfs-ow.gz"), str(root/disk)]
        if via_cli:
            argv = [str(helper.parent/"ow"), "--data-dir", str(root), "mac-host", mode]
            if mode == "restore": argv += ["--disk", str(root/disk)]
        self.proc = subprocess.Popen(argv,
                                    stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)
        self.pending = b""
        self.log = bytearray()
        self.shutdown = None
        try:
            self.read_until(rb"OW_RESTORE_READY\r?\n" if mode == "restore" else rb"OW_READY arch=aarch64\r?\n", self.READY_TIMEOUT)
        except Exception as error:
            self.stop()
            raise RuntimeError(bytes(self.log[-4096:]).decode(errors="replace")) from error
        self.ready_seconds = time.monotonic() - self.start
    def read_until(self, pattern, timeout=10):
        deadline = time.monotonic() + timeout
        try:
            while True:
                match = re.search(pattern, self.pending)
                if match:
                    found = self.pending[:match.end()]
                    self.pending = self.pending[match.end():]
                    return found.decode(errors="replace")
                remaining = deadline - time.monotonic()
                if remaining <= 0: raise TimeoutError(pattern)
                for key, _ in self.selector.select(min(0.25, remaining)):
                    chunk = os.read(key.fd, 65536)
                    if not chunk: raise RuntimeError("guest exited before response")
                    # Retain only a diagnostic tail; bound unmatched output
                    # before concatenating it, even when the guest is noisy.
                    self.log.extend(chunk)
                    del self.log[:-self.LOG_LIMIT]
                    if len(self.pending) + len(chunk) > self.MATCH_LIMIT:
                        raise RuntimeError("guest output limit exceeded while waiting for marker")
                    self.pending += chunk
        except BaseException:
            self.stop()
            raise
    def command(self, command):
        token = "OW_RESULT_" + os.urandom(8).hex()
        self.proc.stdin.write((command + "; _rc=$?; printf '\\n"+token+":%s\\n' \"$_rc\"\n").encode())
        result = self.read_until((token + r":([0-9]+)\r?\n").encode())
        code = int(re.search(token+r":([0-9]+)", result)[1])
        return result, code
    def stop(self):
        if self.shutdown is not None: return self.shutdown
        mode = "already_exited"
        try:
            if self.proc.poll() is None:
                # Graceful shutdown is best-effort, including a full/closed
                # stdin pipe. Never let that write prevent signal fallback.
                graceful = False
                try:
                    fd = self.proc.stdin.fileno()
                    os.set_blocking(fd, False)
                    graceful = os.write(fd, b"sync; poweroff -f\n") == len(b"sync; poweroff -f\n")
                except (OSError, ValueError):
                    pass
                if graceful:
                    mode = "guest_request"
                    try: self.proc.wait(timeout=self.STOP_TIMEOUT)
                    except subprocess.TimeoutExpired: pass
                if self.proc.poll() is None:
                    mode = "sigterm"
                    try: self.proc.terminate()
                    except ProcessLookupError: pass
                    try: self.proc.wait(timeout=self.TERMINATE_TIMEOUT)
                    except subprocess.TimeoutExpired:
                        mode = "sigkill"
                        try: self.proc.kill()
                        except ProcessLookupError: pass
                        self.proc.wait(timeout=self.KILL_TIMEOUT)
            assert self.proc.poll() is not None, "owned helper exit unconfirmed"
            # Collect the helper's final lifecycle marker after confirmed exit.
            # Nonblocking reads preserve bounded cleanup if another process
            # still holds an inherited pipe descriptor.
            os.set_blocking(self.proc.stdout.fileno(), False)
            for _ in range(self.MATCH_LIMIT // 65536):
                try: chunk = os.read(self.proc.stdout.fileno(), 65536)
                except BlockingIOError: break
                if not chunk: break
                self.log.extend(chunk)
                del self.log[:-self.LOG_LIMIT]
            marker = re.search(rb"(?m)^ow-vz: guest stopped\r?$", self.log) is not None
            if mode == "guest_request" and marker and self.proc.returncode == 0:
                mode = "guest_poweroff"
            self.shutdown = {"mode": mode, "returncode": self.proc.returncode,
                             "guest_stop_marker": marker}
            return self.shutdown
        finally:
            self.selector.close()
            for stream in (self.proc.stdin, self.proc.stdout):
                try: stream.close()
                except (OSError, ValueError): pass


def verify_restored_state(output, code):
    # RAM and disk must each have their own exact tagged value. A RAM match
    # cannot stand in for the restored disk's contents.
    assert code == 0, (code, output)
    for tag in ("OW_RAM", "OW_DISK"):
        values = re.findall(r"(?m)^" + tag + r"=([^\r\n]*)\r?$", output)
        assert values == ["captured"], (tag, values, output)


def require_guest_poweroff(shutdown):
    assert shutdown == {"mode": "guest_poweroff", "returncode": 0,
                        "guest_stop_marker": True}, shutdown


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--data-dir", type=Path, required=True)
    args=p.parse_args()
    root=args.data_dir.resolve()
    helper=(Path(__file__).resolve().parent.parent / "target/mac-host/ow-vz")
    receipt={"schema":1,"samples":1,"cpus":1,"memory_mib":512,"checks":[],"shutdowns":[]}
    if (root/"checkpoint").exists() or (root/"restored.img").exists():
        raise ValueError("acceptance requires a fresh prepared fixture root; old captures retained")
    guest=None
    logs=[]
    try:
        guest=Guest(helper, root, via_cli=True)
        receipt["boot_seconds"]=guest.ready_seconds
        receipt["checks"].append("native Rust CLI boots signed helper")
        output,code=guest.command("uname -m; uname -r; tty; stty rows 37 cols 111; stty size")
        assert code==0 and "aarch64" in output and "/dev/hvc0" in output and "37 111" in output,output
        receipt["checks"].append("architecture, controlling console, resize")
        _,code=guest.command("sh -c 'exit 7'")
        assert code==7,code
        receipt["checks"].append("exec exit status 7")
        payload=bytes(range(256))*4
        encoded=base64.b64encode(payload).decode()
        _,code=guest.command("printf '%s' '"+encoded+"' | base64 -d > /persist/roundtrip; sync")
        assert code==0
        output,code=guest.command("sha256sum /persist/roundtrip")
        assert code==0 and hashlib.sha256(payload).hexdigest() in output,output
        receipt["checks"].append("binary file SHA256 round trip")
        # A duplicate launch must be refused while this root has its one guest.
        blocked=subprocess.run([str(helper),"boot",str(root/"Image"),str(root/"initramfs-ow.gz"),
                                str(root/"persist.img")],capture_output=True,timeout=5)
        assert blocked.returncode!=0 and b"already in use" in blocked.stderr,blocked.stderr
        receipt["checks"].append("duplicate launch rejected")
        guest.proc.stdin.write(b"sleep 30\n")
        time.sleep(0.3)
        guest.proc.stdin.write(b"\x03")
        output,code=guest.command("echo OW_INTERRUPT_OK")
        assert code==0 and "OW_INTERRUPT_OK" in output,output
        receipt["checks"].append("console Ctrl-C interrupts sleep")
        # Preserve both a shell-only variable and disk contents at capture.
        _,code=guest.command("OW_RAM_PROBE=captured; echo captured > /persist/state; sync")
        assert code==0
        capture_start=time.monotonic()
        guest.proc.send_signal(signal.SIGUSR1)
        guest.read_until(rb"OW_CAPTURE_READY\r?\n", 30)
        receipt["capture_seconds"]=time.monotonic()-capture_start
        receipt["checks"].append("paired paused RAM/disk capture")
        _,code=guest.command("OW_RAM_PROBE=parent; echo parent > /persist/state; sync")
        assert code==0
        shutdown = guest.stop()
        receipt["shutdowns"].append(shutdown)
        require_guest_poweroff(shutdown)
        logs.append(bytes(guest.log));guest=None
        receipt["checks"].append("guest poweroff marker and helper exit0")
        subprocess.run(["/bin/cp", "-c", str(root/"checkpoint/disk.img"), str(root/"restored.img")],check=True)
        guest=Guest(helper,root,"restored.img","restore",via_cli=True)
        receipt["restore_seconds"]=guest.ready_seconds
        output,code=guest.command('printf "\\nOW_RAM=%s\\nOW_DISK=%s\\n" "$OW_RAM_PROBE" "$(cat /persist/state)"')
        verify_restored_state(output, code)
        receipt["checks"].append("RAM variable and paired disk restored")
        _,code=guest.command("echo restored > /persist/state; sync")
        assert code==0
        shutdown = guest.stop()
        receipt["shutdowns"].append(shutdown)
        require_guest_poweroff(shutdown)
        logs.append(bytes(guest.log));guest=None
        guest=Guest(helper,root)
        output,code=guest.command("sha256sum /persist/roundtrip")
        assert code==0 and hashlib.sha256(payload).hexdigest() in output,output
        receipt["checks"].append("cold boot disk persistence")
        output,code=guest.command("cat /persist/state")
        assert code==0 and "parent" in output and "restored" not in output,output
        receipt["checks"].append("captured disk clone write independence")
        # Test owned host signal cleanup separately from guest shutdown.
        guest.proc.terminate();guest.proc.wait(timeout=5)
        assert guest.proc.returncode==0
        receipt["shutdowns"].append({"requested": "host_sigterm", **guest.stop()})
        receipt["checks"].append("host SIGTERM stop and confirmed exit")
    finally:
        if guest:
            guest.stop();logs.append(bytes(guest.log))
        (root/"acceptance.log").write_bytes(b"\n--- cold boot ---\n".join(logs))
    rejected=subprocess.run([str(helper),"restore",str(root/"Image"),str(root/"initramfs-ow.gz"),
                             str(root/"persist.img")],capture_output=True,timeout=5)
    assert rejected.returncode!=0 and b"hash/configuration mismatch" in rejected.stderr,rejected.stderr
    receipt["checks"].append("restore with newer unpaired disk rejected")
    subprocess.run(["/bin/cp", "-c", str(root/"checkpoint/disk.img"), str(root/"tamper-test.img")],check=True)
    state=root/"checkpoint/state.vz"
    with state.open("r+b") as file:
        original=file.read(1); file.seek(0); file.write(bytes([original[0]^1]))
    try:
        rejected=subprocess.run([str(helper),"restore",str(root/"Image"),str(root/"initramfs-ow.gz"),
                                 str(root/"tamper-test.img")],capture_output=True,timeout=5)
        assert rejected.returncode!=0 and b"hash/configuration mismatch" in rejected.stderr,rejected.stderr
        receipt["checks"].append("corrupt memory capture rejected before restore")
    finally:
        with state.open("r+b") as file: file.write(original)
    caps=json.loads(subprocess.check_output([str(helper),"capabilities"]))
    assert not caps["memory_snapshot"] and not caps["pool_enrollment"]
    receipt["capabilities"]=caps
    receipt["checks"].append("unimplemented capabilities disabled")
    (root/"acceptance.json").write_text(json.dumps(receipt,indent=2)+"\n")
    print(json.dumps(receipt,indent=2))

if __name__=="__main__": main()
