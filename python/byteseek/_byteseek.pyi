# The package's public names are typed in `__init__.pyi`; this covers what only the
# package itself imports from the extension module.

def run_cli(args: list[str]) -> int: ...
