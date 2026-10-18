"""Narrow privilege boundary: fixed target, preserved listeners, no public ingress."""
import copy
import runpy
import unittest
from pathlib import Path

helper = runpy.run_path(str(Path(__file__).parent / "files/preview-serve"))


class Preview(unittest.TestCase):
    def test_routes_preserve_other_listeners_and_reject_collisions(self):
        route = helper["route_commands"]
        config = {"TCP": {"443": {"HTTPS": True}}, "Web": {"host.net.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8787"}}}}}
        before = copy.deepcopy(config)
        commands = route(config, "host.net.ts.net", 11000, True)
        self.assertEqual(commands, [["/usr/bin/tailscale", "serve", "--bg", "--yes", "--https=11000", "http://127.0.0.1:8787"]])
        self.assertEqual(config, before)
        config["TCP"]["11000"] = {"HTTPS": True}
        config["Web"]["host.net.ts.net:11000"] = copy.deepcopy(config["Web"]["host.net.ts.net:443"])
        self.assertEqual(route(config, "host.net.ts.net", 11000, True), [])
        self.assertEqual(route(config, "host.net.ts.net", 11000, False), [["/usr/bin/tailscale", "serve", "--https=11000", "off"]])
        for bad in [
            {**config, "AllowFunnel": {"host.net.ts.net:11000": True}},
            {**config, "TCP": {"11000": {"TCPForward": "localhost:22"}}},
            {**config, "Web": {"host.net.ts.net:11000": {"Handlers": {"/other": {"Text": "keep"}}}}},
            {**config, "Web": {**config["Web"], "other.net.ts.net:11000": {}}},
        ]:
            with self.assertRaises(ValueError):
                route(bad, "host.net.ts.net", 11000, True)
            with self.assertRaises(ValueError):
                route(bad, "host.net.ts.net", 11000, False)

    def test_bad_arguments_fail_before_any_command(self):
        for args in [[], ["443", "on"], ["8443", "on"], ["10000", "on"], ["65536", "on"], ["11000", "reset"], ["+11000", "on"], ["011000", "on"], ["11000", "on", "extra"]]:
            with self.assertRaises(ValueError):
                helper["main"](args)
