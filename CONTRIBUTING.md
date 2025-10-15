# Contributions Welcome

Firecracker is running serverless workloads at scale within AWS, but it's still
day 1 on the journey guided by our [mission](CHARTER.md). There's a lot more to
build and we welcome all contributions.

There's a lot to contribute to in Firecracker. We've opened issues for all the
features we want to build and improvements we want to make. Good first issues
are labeled accordingly. We're also keen to hearing about your use cases and how
we can support them, your ideas, and your feedback for what's already here.

If you're just looking for quick feedback for an idea or proposal, open an
[issue](https://github.com/firecracker-microvm/firecracker/issues) or chat with
us on the [Firecracker Slack workgroup](https://firecracker-microvm.slack.com).

Follow the [contribution workflow](#contribution-workflow) for submitting your
changes to the Firecracker codebase. If you want to receive high-level but still
commit-based feedback for a contribution, follow the
[request for comments](#request-for-comments) steps instead.

## Contribution Workflow

Firecracker uses the “fork-and-pull” development model. Follow these steps if
you want to merge your changes to Firecracker:

1. Within your fork of
   [Firecracker](https://github.com/firecracker-microvm/firecracker), create a
   branch for your contribution. Use a meaningful name.
1. Create your contribution, meeting all
   [contribution quality standards](#contribution-quality-standards)
1. [Create a pull request](https://help.github.com/articles/creating-a-pull-request-from-a-fork/)
   against the main branch of the Firecracker repository.
1. Add two reviewers to your pull request (a maintainer will do that for you if
   you're new). Work with your reviewers to address any comments and obtain a
   minimum of 2 approvals from [maintainers](MAINTAINERS.md). To update your
   pull request, amend existing commits whenever applicable. Then force-push the
   new changes to your pull request branch. Address all review comments you
   receive.
1. Once the pull request is approved, one of the maintainers will merge it.

## Request for Comments

If you just want to receive feedback for a contribution proposal, open an “RFC”
(“Request for Comments”) pull request:

1. On your fork of
   [Firecracker](https://github.com/firecracker-microvm/firecracker), create a
   branch for the contribution you want feedback on. Use a meaningful name.
1. Create your proposal based on the existing codebase.
1. [Create a pull request](https://help.github.com/articles/creating-a-pull-request-from-a-fork/)
   against the main branch of the Firecracker repository. Prefix your pull
   request name with `[RFC]`.
1. Discuss your proposal with the community on the pull request page (or on any
   other channel). Add the conclusion(s) of this discussion to the pull request
   page.

## Contribution Quality Standards

### Commits

Your contribution needs to meet the following standards:

- Separate each **logical change** into its own commit.
- Each commit must pass all unit & code style tests, and the full pull request
  must pass all integration tests. See [tests/README.md](tests/README.md) for
  information on how to run tests.
- Unit test coverage must _increase_ the overall project code coverage.
- Include integration tests for any new functionality in your pull request.
- Document all your public functions.
- Add a descriptive message for each commit. Follow
  [commit message best practices](https://github.com/erlang/otp/wiki/writing-good-commit-messages).
- A good commit message may look like
  ```
  A descriptive title of 72 characters or fewer

  A concise description where each line is 72 characters or fewer.

  Signed-off-by: <A full name> <A email>
  Co-authored-by: <B full name> <B email>
  ```
- Document your pull requests. Include the reasoning behind each change, and the
  testing done.
- Acknowledge Firecracker's [Apache 2.0 license](LICENSE) and certify that no
  part of your contribution contravenes this license by signing off on all your
  commits with `git -s`. Ensure that every file in your pull request has a
  header referring to the repository license file.

### Code formatting

Most code style standards are enforced automatically by code formatter we use.

- Rust - `rust fmt`
- Python - `black`, `isort` and `pylint`
- Markdown - `mdlint`

Most quality and style standards are enforced automatically during integration
testing. For ease of use you can set up a git pre-commit hook by running the
following in the Firecracker root directory:

```
cat >> .git/hooks/pre-commit << EOF
./tools/devtool checkstyle || exit 1
./tools/devtool checkbuild --all || exit 1
EOF
```

The first command will automatically lint your Rust, markdown and python changes
when running `git commit`, as well as running any other checks our CI validates
as part of its 'Style' step. Most reported violations can be automatically fixed
using `./tools/devtool fmt`. The second command will then check that the code
correctly compiles on all supported architectures, and that it passes Rust
clippy rules defined for the project.

### Rust style

- Don't use `unsafe` until absolutely necessary. If `unsafe` is required, it
  should be accompanied by a comment with:

  - Justification, potentially including quantifiable reasons why safe
    alternatives were not used
  - Safety comment, as per clippy::undocumented_unsafe_blocks. This comment must
    list all invariants of the called function, and explain why there are
    upheld. If relevant, it must also prove that undefined behavior is not
    possible.

  Example:
  https://github.com/firecracker-microvm/firecracker/blob/main/src/vmm/src/devices/virtio/iov_deque.rs#L118

- Prefer `for` loops over `for_each`/`try_for_each`. This makes code easier to
  reason about.

### Errors and asserts

#### Errors

There are 3 main entities with which Firecracker interacts:

- Host Kernel
- User
- Guest Kernel

All of the interaction points with these entities are potential points of
failure. In Firecracker we prefer to handle these failure points gracefully, in
other words, we prefer to return errors and propagate them up the call stack,
making them visible to the requesting API user (`User` or `Guest OS`).

The example API call flow with the error propagation:

```
User -> Add block device -> Firecracker -> Try open a backing file -> Host Kernel -> No such file
Response  <-------------------------------------------------------------------------------------|
```

This philosophy means we use `Result<... , ...>` types extensively across the
codebase. There are some rules when it comes to it's usage:

- Don't redefine `Result` type Some libraries and even standard library
  sometimes redefines `Result<..., ...>` like this:

  ```
  pub type Result<T> = result::Result<T, Error>;
  ```

  We find this pattern counter intuitive as it introduces additional indirection
  jump needed to understand function definition.

- Don't create unnecessary error types/enums. Sometimes the best way to deal
  with errors is to no have them. Some failure can be handled in place where
  they occur and do not require to be propagated up the call stack. Example:
  https://github.com/firecracker-microvm/firecracker/blob/main/src/vmm/src/arch/aarch64/vcpu.rs#L51

#### Asserts

Asserts are not a way of error handling, but a way of error prevention. They are
a sanity checks placed in code, ensuring the correctness of assumptions, code is
operating with. The usage of asserts is allowed everywhere, but in general they
are most useful in the code paths that do not interact with host kernel/user or
guest kernel. Rules for assert usage:

- Assert invariants function is operating with.
- Use asserts as sanity checks if needed. Sometimes it is worth adding an assert
  on the obviously true statement as a stronger guarantee if correctness.
- Assert one condition at a time. Don't `assert!(a && b)` as it will be hard to
  understand which condition failed.

Example:
https://github.com/firecracker-microvm/firecracker/blob/main/src/vmm/src/pci/configuration.rs#L246

#### Converting Error code into Assert code

Firecracker holds a very strict security and correctness guarantees. But as all
things, they can be improved over time. Since usage of assertions increase the
risk of the program stopping unexpectedly, there is a higher pressure for
developers to ensure the code is correct and does not cause program to crash. As
a result, assertions are one of the tools we can uslize to improve the security
and correctness of the Firecracker.

Keeping this in mind, here are general rules for writing new code or updating
old one:

- Assert by default: use `assert!`/`unwrap`/`expect`
- If possible validate all input from user or guest before proceeding: this
  allows the code that follows the validation to use asserts more freely
- If the error is a result of a user invalid input/request -> return `Result`
- If the error is a result of a guest invalid action/request -> return `Result`
- If the error is a result of a system failure (syscall failure) -> return
  `Result`

## Developer Certificate of Origin

Firecracker is an open source product released under the
[Apache 2.0 license](LICENSE).

We respect intellectual property rights of others and we want to make sure all
incoming contributions are correctly attributed and licensed. A Developer
Certificate of Origin (DCO) is a lightweight mechanism to do that.

The DCO is a declaration attached to every contribution made by every developer.
In the commit message of the contribution, the developer simply adds a
`Signed-off-by` statement and thereby agrees to the DCO, which you can find
below or at DeveloperCertificate.org (<http://developercertificate.org/>).

```
Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the
    best of my knowledge, is covered under an appropriate open
    source license and I have the right under that license to
    submit that work with modifications, whether created in whole
    or in part by me, under the same open source license (unless
    I am permitted to submit under a different license), as
    Indicated in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including
    all personal information I submit with it, including my
    sign-off) is maintained indefinitely and may be redistributed
    consistent with this project or the open source license(s)
    involved.
```

We require that every contribution to Firecracker is signed with a Developer
Certificate of Origin. DCO checks are enabled via <https://github.com/apps/dco>,
and your PR will fail CI without it.

Additionally, we kindly ask you to use your real name. We do not accept
anonymous contributors, nor those utilizing pseudonyms. Each commit must include
a DCO which looks like this:

```
Signed-off-by: Jane Smith <jane.smith@email.com>
```

You may type this line on your own when writing your commit messages. However,
if your `user.name` and `user.email` are set in your git config, you can use
`-s` or `--signoff` to add the `Signed-off-by` line to the end of the commit
message automatically.

Forgot to add DCO to a commit? Amend it with `git commit --amend -s`.
