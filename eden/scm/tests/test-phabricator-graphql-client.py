# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import unittest
from unittest.mock import patch

import silenttestrunner
from sapling import encoding, ui as uimod
from sapling.ext.extlib.phabricator import graphql, phabricator_graphql_client_urllib

METADATA = "id=agentcloud,session_id=0b8f2c1e-5d2a-4f7b-9c3e-1a2b3c4d5e6f"


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


if __name__ == "__main__":
    silenttestrunner.main(__name__)
