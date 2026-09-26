# test-waits-on-sleep
globs: **/*_test.go, **/*.test.ts, **/*.test.tsx, **/*.spec.ts, **/tests/**/*.rs, **/test_*.py, **/*_test.py

Does a test in `content` wait for something to happen by sleeping, or by polling in a loop with a timer, instead of waiting on a real signal such as a channel, event, callback, or returned value?

# test-only-checks-mocks
globs: **/*_test.go, **/*.test.ts, **/*.test.tsx, **/*.spec.ts, **/tests/**/*.rs, **/test_*.py, **/*_test.py

Does a test in `content` only assert on mocks or stubs it set up itself, so it would still pass if the real code under test were broken?

# test-checks-what-the-compiler-guarantees
globs: **/*_test.go, **/*.test.ts, **/*.test.tsx, **/*.spec.ts, **/tests/**/*.rs

Does a test in `content` only check something the type system or compiler already guarantees, such as a field existing, a constructor returning the declared type, or a constant equaling its literal value?

# test-cannot-fail
globs: **/*_test.go, **/*.test.ts, **/*.test.tsx, **/*.spec.ts, **/tests/**/*.rs, **/test_*.py, **/*_test.py

Does a test in `content` have no assertion that could fail, for example it has no assertions at all, swallows errors, or asserts only that code ran without checking any result?
