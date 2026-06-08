"""CLI entry point for basemind."""

import sys

from .downloader import run_basemind


def main():
    """Main entry point for the CLI."""
    args = sys.argv[1:]
    run_basemind(args)


if __name__ == "__main__":
    main()
