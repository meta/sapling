# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import contextlib
import ctypes
import importlib
import sys
import types
import unittest
from unittest.mock import MagicMock, patch

import silenttestrunner
from sapling import encoding, ui as uimod, util
from sapling.ext.extlib.phabricator import graphql, phabricator_graphql_client_urllib

METADATA = "id=agentcloud,session_id=0b8f2c1e-5d2a-4f7b-9c3e-1a2b3c4d5e6f"
WSAENETDOWN = 10050


@contextlib.contextmanager
def windows_socket_module(ws2_32):
    """sapling.windows_socket on any platform, with `ws2_32` as its Winsock DLL."""
    wintypes = types.ModuleType("ctypes.wintypes")
    wintypes.WORD, wintypes.HANDLE = ctypes.c_ushort, ctypes.c_void_p
    wintypes.DWORD = ctypes.c_ulong
    with (
        patch.object(ctypes, "windll", MagicMock(ws2_32=ws2_32), create=True),
        patch.object(ctypes, "wintypes", wintypes, create=True),
        patch.dict(sys.modules, {"ctypes.wintypes": wintypes}),
    ):
        sys.modules.pop("sapling.windows_socket", None)
        yield importlib.import_module("sapling.windows_socket")


class QueryHeadersTest(unittest.TestCase):
    def setUp(self):
        self.ui = uimod.ui()
        self.ui.setconfig("auth_proxy", "unix_socket_path", "/nonexistent/socket")
        self.ui.setconfig("phabricator", "graphql_app_id", "1")
        self.ui.setconfig("phabricator", "graphql_host", "https://phabricator.test")

    def sent_headers(self, metadata):
        requests = phabricator_graphql_client_urllib.PhabricatorGraphQLClientRequests
        with (
            patch.dict(encoding.environ),
            patch.object(requests, "sendpost", return_value={"data": {}}) as sendpost,
        ):
            encoding.environ.pop("CODING_AGENT_METADATA", None)
            if metadata is not None:
                encoding.environ["CODING_AGENT_METADATA"] = metadata
            graphql.Client(repodir="/", ui=self.ui).graphqlquery("query { x }", {})
        return sendpost.call_args.kwargs["headers"]

    def test_a_query_forwards_the_coding_agent_metadata(self):
        headers = self.sent_headers(METADATA)
        self.assertEqual(headers.get("X-Coding-Agent-Metadata"), METADATA)

    def test_a_query_sends_no_metadata_when_the_variable_is_unset(self):
        self.assertNotIn("X-Coding-Agent-Metadata", self.sent_headers(None))

    def test_a_query_sends_no_metadata_when_the_config_is_off(self):
        self.ui.setconfig("phabricator", "send-coding-agent-metadata", False)
        self.assertNotIn("X-Coding-Agent-Metadata", self.sent_headers(METADATA))

    def test_a_query_drops_metadata_with_a_control_character(self):
        headers = self.sent_headers("id=agentcloud\r\nX-Injected: 1")
        self.assertNotIn("X-Coding-Agent-Metadata", headers)

    def test_a_query_drops_metadata_that_is_not_ascii(self):
        headers = self.sent_headers("id=agentcloud,session_id=\u4e2d")
        self.assertNotIn("X-Coding-Agent-Metadata", headers)

    def cats_header(self, status, body=b"TOKENS\n"):
        self.ui.setconfig("phabricator", "x2p-cat-verifiers", "USER:a, SERVICE:b")
        httpclient = phabricator_graphql_client_urllib.httpclient
        with patch.object(httpclient, "HTTPConnection") as connect:
            response = connect.return_value.getresponse.return_value
            response.status, response.read.return_value = status, body
            headers = self.sent_headers(None)
        self.assertEqual(connect.call_args.args, ("cats.x2pagentd.local",))
        connect.return_value.request.assert_called_once_with(
            "GET", b"/create?verifiers=USER%3Aa%2CSERVICE%3Ab"
        )
        return headers.get("crypto_auth_tokens")

    def test_a_query_carries_the_cats_the_auth_proxy_mints(self):
        self.assertEqual(self.cats_header(200), "TOKENS")

    def test_a_query_is_sent_without_cats_when_the_auth_proxy_refuses(self):
        self.assertIsNone(self.cats_header(500))

    def test_a_query_is_sent_without_cats_when_the_auth_proxy_answers_garbage(self):
        self.assertIsNone(self.cats_header(200, b"\xff"))

    def test_a_query_is_sent_without_cats_when_the_auth_proxy_is_unreachable(self):
        self.ui.setconfig("phabricator", "x2p-cat-verifiers", "USER:a")
        self.assertNotIn("crypto_auth_tokens", self.sent_headers(None))

    def test_a_query_is_sent_without_cats_when_the_proxy_socket_fails_on_windows(self):
        self.ui.setconfig("phabricator", "x2p-cat-verifiers", "USER:a")
        ws2_32 = MagicMock()
        ws2_32.WSAStartup.return_value = 0
        ws2_32.socket.return_value = 3
        ws2_32.setsockopt.return_value = 0
        ws2_32.connect.return_value = -1
        ws2_32.WSAGetLastError.return_value = WSAENETDOWN
        with windows_socket_module(ws2_32) as windows_socket:
            with patch.object(util, "unixsocket", windows_socket.WindowsSocketHandle):
                headers = self.sent_headers(None)
        ws2_32.connect.assert_called_once()
        self.assertNotIn("crypto_auth_tokens", headers)


class SendPostTest(unittest.TestCase):
    def _post(
        self,
        url,
        unix_socket_proxy=None,
        identity=None,
        service_id=None,
        use_interngraph=True,
    ):
        requests = phabricator_graphql_client_urllib.PhabricatorGraphQLClientRequests(
            unix_socket_proxy=unix_socket_proxy
        )
        # None stands in for the open-source build, where the fb package
        # is absent and the guarded import yields None.
        stub = (
            types.SimpleNamespace(resolve_client_identity=lambda: identity)
            if use_interngraph
            else None
        )
        httpclient = phabricator_graphql_client_urllib.httpclient
        with (
            patch.object(phabricator_graphql_client_urllib, "_interngraph", stub),
            patch.object(httpclient, "HTTPConnection") as connect,
            patch.dict(encoding.environ),
        ):
            encoding.environ.pop("FB_SERVICE_ID", None)
            if service_id is not None:
                encoding.environ["FB_SERVICE_ID"] = service_id
            connect.return_value.getresponse.return_value.read.return_value = b"{}"
            requests.sendpost(url, data={"doc": "query"}, timeout=30)
        return connect

    def test_sendpost_sets_the_sapling_user_agent(self):
        connect = self._post("https://phabricator.test/graphql")
        headers = connect.return_value.request.call_args.args[3]
        self.assertEqual(headers["User-Agent"], "sapling")

    def test_sendpost_appends_the_service_id_to_the_user_agent(self):
        connect = self._post("https://phabricator.test/graphql", service_id="123")
        headers = connect.return_value.request.call_args.args[3]
        self.assertEqual(headers["User-Agent"], "sapling FBSVCID/123")

    def test_sendpost_drops_a_service_id_that_is_not_ascii(self):
        connect = self._post("https://phabricator.test/graphql", service_id="\u670d")
        headers = connect.return_value.request.call_args.args[3]
        self.assertEqual(headers["User-Agent"], "sapling")

    def test_sendpost_passes_local_certs_for_direct_https(self):
        connect = self._post(
            "https://phabricator.test/graphql",
            identity=("/tmp/cert.pem", "/tmp/key.pem"),
        )
        self.assertEqual(
            connect.call_args.kwargs,
            {
                "timeout": 30,
                "use_ssl": True,
                "certfile": "/tmp/cert.pem",
                "keyfile": "/tmp/key.pem",
            },
        )

    def test_sendpost_passes_no_certs_without_a_local_identity(self):
        connect = self._post("https://phabricator.test/graphql", identity=None)
        self.assertNotIn("certfile", connect.call_args.kwargs)
        self.assertNotIn("keyfile", connect.call_args.kwargs)

    def test_sendpost_passes_no_certs_when_internal_module_absent(self):
        connect = self._post("https://phabricator.test/graphql", use_interngraph=False)
        self.assertNotIn("certfile", connect.call_args.kwargs)
        self.assertNotIn("keyfile", connect.call_args.kwargs)

    def test_sendpost_passes_no_certs_over_the_unix_socket_proxy(self):
        connect = self._post(
            "https://phabricator.test/graphql",
            unix_socket_proxy="/nonexistent/socket",
            identity=("/tmp/cert.pem", "/tmp/key.pem"),
        )
        self.assertNotIn("certfile", connect.call_args.kwargs)
        self.assertNotIn("keyfile", connect.call_args.kwargs)

    def test_sendpost_passes_no_certs_for_plain_http(self):
        connect = self._post(
            "http://phabricator.test/graphql",
            identity=("/tmp/cert.pem", "/tmp/key.pem"),
        )
        self.assertNotIn("certfile", connect.call_args.kwargs)
        self.assertNotIn("keyfile", connect.call_args.kwargs)


if __name__ == "__main__":
    silenttestrunner.main(__name__)
