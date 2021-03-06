# Common helpers
from __future__ import print_function, unicode_literals
import sys
import json
import subprocess
import exceptions


class fail_quietly_unless_explicit_success:
    """
    Suppress exception tracebacks, and fail with an error exit code,
    as is appropriate for assertion scripts. Only exit with success
    explicitly.
    """

    def print_and_exit(self, *args):
        print(*args)
        sys.exit(0)

    def __enter__(self):
        return self.print_and_exit

    def __exit__(self, type_, value_, traceback_):
        if type_ == exceptions.SystemExit:
            sys.exit(value_)
        # If we get here, we are exiting the block without having
        # called `succeed`, so regardless of exception status we
        # want to exit with a failure code
        sys.exit(1)


class AWSCLIError(AssertionError):
    pass


def call_aws(*subcommands):
    try:
        args = [
            "aws",
            # Recall that this script is expected to run in-cluster
            "--endpoint-url=http://localstack.default:4566",
        ]
        args.extend(subcommands)
        output = subprocess.check_output(args)
        parsed = json.loads(output)
        return parsed
    except (subprocess.CalledProcessError, ValueError) as e:
        raise AWSCLIError(e)
