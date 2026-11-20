"""Cross-file resolution of each import form: dotted, aliased, renamed, re-exported, submodule."""
import pkg.mod
import pkg.mod as alias_mod
from pkg import helper
from pkg import views
from pkg.mod import fn as renamed


def calls_dotted_module():
    return pkg.mod.fn()


def calls_aliased_module():
    return alias_mod.fn()


def calls_renamed_function():
    return renamed()


def calls_reexported_helper():
    return helper()


def calls_submodule():
    return views.render()
