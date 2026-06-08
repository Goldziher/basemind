"""CLI entry point for gitmind."""

import sys

from .downloader import run_gitmind


def main():
    """Main entry point for the CLI."""
    args = sys.argv[1:]
    run_gitmind(args)


if __name__ == "__main__":
    main()
