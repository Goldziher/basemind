from mod import f

x = "module x"


def uses_local_shadow():
    x = "local x"
    return x


def first_param(x):
    return x


def second_param(x):
    return x * 2


def comprehension_shadow():
    x = "outer x"
    values = [x for x in range(3)]
    return x, values


def calls_imported_f():
    return f()
