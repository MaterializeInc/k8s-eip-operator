#!/usr/bin/python2.7
# Python2! I know, but it's what's available in the AWS CLI container, OK?
from __future__ import print_function, unicode_literals
import sys
import json

from _common import fail_quietly_unless_explicit_success

with fail_quietly_unless_explicit_success() as succeed:
    eips = json.load(sys.stdin)
    for e in eips["items"]:
        if e["metadata"]["name"].startswith("materialized-"):
            succeed("Found EIP: ", e)
