#!/usr/bin/env python3
"""Real GnuPG negative regressions; no download or VM, also run under python -O."""

import importlib.util, pathlib, subprocess, tempfile, unittest, io, tarfile

spec = importlib.util.spec_from_file_location(
    "prepare", pathlib.Path(__file__).with_name("prepare-mac-node.py")
)
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


class Trust(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.home = self.root / "keys"
        self.home.mkdir(mode=0o700)
        self.good = self.key("trusted")
        self.bad = self.key("spoof " + p.UEC_FINGERPRINT)
        self.pin = p.UEC_FINGERPRINT
        self.addCleanup(setattr, p, "UEC_FINGERPRINT", self.pin)
        self.document = self.root / "SHA256SUMS"
        self.document.write_text("signed manifest\n")
        self.keyring = self.root / "release.gpg"
        self.signature = self.root / "SHA256SUMS.gpg"

    def gpg(self, *args):
        return subprocess.check_output(
            ["gpg", "--no-options", "--batch", "--homedir", str(self.home), *args],
            stderr=subprocess.DEVNULL,
        )

    def key(self, uid):
        self.gpg(
            "--passphrase", "", "--quick-generate-key", uid, "ed25519", "sign", "0"
        )
        records = self.gpg("--with-colons", "--list-keys", uid).decode().splitlines()
        return next(x.split(":")[9] for x in records if x.startswith("fpr:"))

    def sign(self, fingerprint):
        self.gpg(
            "--local-user",
            fingerprint,
            "--output",
            str(self.signature),
            "--detach-sign",
            str(self.document),
        )

    def test_spoofed_uid_is_not_fingerprint(self):
        self.keyring.write_bytes(self.gpg("--export", self.bad))
        self.sign(self.bad)
        with self.assertRaises(ValueError):
            p.verify_release(self.keyring, self.signature, self.document)

    def test_extra_key_cannot_sign(self):
        p.UEC_FINGERPRINT = self.good
        self.keyring.write_bytes(self.gpg("--export"))
        self.sign(self.bad)
        with self.assertRaises(subprocess.CalledProcessError):
            p.verify_release(self.keyring, self.signature, self.document)

    def test_pinned_signer_is_accepted_with_extra_key(self):
        p.UEC_FINGERPRINT = self.good
        self.keyring.write_bytes(self.gpg("--export"))
        self.sign(self.good)
        p.verify_release(self.keyring, self.signature, self.document)

    def test_archive_keyring_must_match_authenticated_root(self):
        root = self.root / "root.tar.xz"
        contents = b"authenticated archive trust"
        with tarfile.open(root, "w:xz") as tar:
            member = tarfile.TarInfo("usr/share/keyrings/ubuntu-archive-keyring.gpg")
            member.size = len(contents)
            tar.addfile(member, io.BytesIO(contents))
        supplied = self.root / "archive.gpg"
        supplied.write_bytes(b"replaced")
        with self.assertRaises(ValueError):
            p.authenticated_archive_keyring(root, supplied)
        supplied.write_bytes(contents)
        self.assertEqual(p.authenticated_archive_keyring(root, supplied), contents)


if __name__ == "__main__":
    unittest.main()
