# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.


import base64

from sapling import json

from . import arcconfig


class PhabricatorGraphQLClient:
    def __init__(
        self, urllib, x2p_app_id, ph_oauth, ph_cats, host, coding_agent_metadata=None
    ):
        self.urllib = urllib
        self.phabricator_oauth = ph_oauth
        self.phabricator_cats = ph_cats
        self.graphql_url = host + "/graphql"
        self.x2p_app_id = x2p_app_id
        self.coding_agent_metadata = coding_agent_metadata

    def query(self, timeout, request, params=None):
        """
        Make a graphql2 (OSS) request to phabricator data
        """

        headers = {}
        if self.x2p_app_id is not None:
            headers = {
                "x-x2pagentd-inject-cat": base64.b64encode(
                    json.dumps(
                        {
                            "isIntern": True,
                            "appId": int(self.x2p_app_id),
                            "tokenTimeoutSeconds": 60,
                        }
                    ).encode()
                )
            }
            # The injected CAT is only good for the GraphQL endpoint. A service
            # the endpoint calls for us needs its own to know who is asking.
            cats = self.urllib.mintcats(timeout)
            if cats:
                headers["crypto_auth_tokens"] = cats
            data = {
                "doc": request,
                "variables": params,
            }
        elif self.phabricator_oauth is not None:
            data = {
                "access_token": self.phabricator_oauth,
                "doc": request,
                "variables": params,
            }
        elif self.phabricator_cats is not None:
            data = {
                "crypto_auth_tokens": self.phabricator_cats,
                "cat_app": 197058370321847,
                "doc": request,
                "variables": params,
            }
        else:
            raise arcconfig.ArcRcMissingCredentials(
                "The arcrc was missing valid authentication (either OAuth or CATs). "
                "For humans, follow the instructions at "
                "https://www.internalfb.com/intern/jf/authenticate/ "
                "to get a Phabricator OAuth token. "
                "For bots, see http://fburl.com/botdiffs for more info."
            )

        if self.coding_agent_metadata:
            headers["X-Coding-Agent-Metadata"] = self.coding_agent_metadata

        return self.urllib.sendpost(
            self.graphql_url,
            data=data,
            timeout=timeout,
            headers=headers,
        )
