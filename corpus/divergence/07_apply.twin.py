# CPython twin of 07_apply.oro: the same program with `apply(f, args=xs)`
# written `f(*xs)` and `apply(f, kwargs=d)` written `f(**d)`. Its output is the
# `.expected` file, so the Oro program is still oracled against CPython even
# though CPython cannot run it.


def total(nums):
    t = 0
    for n in nums:
        t = t + n
    return t


def describe(name, tags, w=0, h=0):
    out = name
    for t in tags:
        out = out + " " + t
    return out + f" w={w} h={h}"


def forward(name, tags, opts):
    return describe(*[name, tags], **opts)


print(total([1, 2, 3, 4]))
print(total([]))
print(describe("box", ["red", "small"], w=3, h=4))
print(forward("box", ["blue"], {"h": 9}))
print(total(*[[5, 6]]))
print(describe(*["bare", []]))
