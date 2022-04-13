#!/usr/bin/python2.7
# Python2! I know, but it's what's available in the AWS CLI container, OK?
from __future__ import print_function, unicode_literals

from _common import call_aws, fail_quietly_unless_explicit_success

with fail_quietly_unless_explicit_success() as succeed:
    addresses = call_aws("ec2", "describe-addresses")
    if len(addresses["Addresses"]) == 0:
        succeed("All addresses cleaned up!")
