import sys


def main(argv: list[str] | None = None) -> int:
    print("forktest scaffold", argv if argv is not None else sys.argv[1:])
    return 0
