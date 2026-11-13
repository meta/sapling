# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.


import http.client as httplib

from sapling import httpclient, json, util

urlreq = util.urlreq

# helper class so phabricator_graphql_client can talk using the requests
# third-party library


class PhabricatorClientError(Exception):
    def __init__(self, reason, error):
        Exception.__init__(self, reason, error)


class InvalidJSONResponse(Exception):
    def __init__(self, status, reason, raw_data):
        self.status = status
        self.reason = reason
        self.raw_data = raw_data
        message = "Invalid JSON response from Phabricator (HTTP %s %s): %r" % (
            status,
            reason,
            raw_data[:1000],
        )
        Exception.__init__(self, message)


class PhabricatorGraphQLClientRequests:
    def __init__(self, unix_socket_proxy=None, ui=None):
        self._connection = None
        self._unix_socket_proxy = unix_socket_proxy
        self._ui = ui

    def __verify_connection(self, request_url, timeout):
        urlparts = urlreq.urlparse(request_url)

        if self._connection is None:
            if self._unix_socket_proxy:
                self._connection = httpclient.HTTPConnection(
                    urlparts.hostname,
                    unix_socket_path=self._unix_socket_proxy,
                    timeout=timeout,
                )
            elif urlparts.scheme == "http":
                self._connection = httpclient.HTTPConnection(
                    urlparts.netloc, timeout=timeout
                )
            elif urlparts.scheme == "https":
                self._connection = httpclient.HTTPConnection(
                    urlparts.netloc,
                    timeout=timeout,
                    use_ssl=True,
                )
            else:
                raise PhabricatorClientError("Unknown host scheme: %s", urlparts.scheme)
        return urlparts

    def mintcats(self, timeout):
        """CATs from the auth proxy for `phabricator.x2p-cat-verifiers`, if any."""
        ui = self._ui
        verifiers = ui and ui.configlist("phabricator", "x2p-cat-verifiers")
        if not verifiers:
            return None
        conn = httpclient.HTTPConnection(
            "cats.x2pagentd.local",
            unix_socket_path=self._unix_socket_proxy,
            timeout=timeout,
        )
        query = urlreq.urlencode({"verifiers": ",".join(verifiers)})
        try:
            conn.request("GET", b"/create?" + query)
            res = conn.getresponse()
            body = res.read().decode("ascii").strip()
            if res.status == 200:
                return body
            ui.debug("auth proxy cannot mint CATs: HTTP %s %s\n" % (res.status, body))
        except (OSError, ValueError, httplib.HTTPException) as ex:
            ui.debug("auth proxy cannot mint CATs: %r\n" % ex)
        finally:
            conn.close()
        return None

    def sendpost(self, request_url, data, timeout, headers=None):
        urlparts = self.__verify_connection(request_url, timeout)
        query = util.urlreq.urlencode(data)

        headers = {
            "Connection": "Keep-Alive",
            "Content-Type": "application/x-www-form-urlencoded",
            **(headers or {}),
        }
        self._connection.request("POST", urlparts.path, query, headers)
        ui = self._ui
        try:
            res = self._connection.getresponse()
            raw_data = res.read()
            ret = json.loads(raw_data)
        except json.JSONDecodeError as e:
            if ui is not None:
                ui.debug("Response headers: %r\n" % res.getheaders())
                ui.debug("Raw response: %s\n" % raw_data)
            raise InvalidJSONResponse(
                getattr(res, "status", "unknown"),
                getattr(res, "reason", "unknown"),
                raw_data,
            ) from e
        return ret
