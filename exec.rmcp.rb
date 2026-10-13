# typed: true
# exec: the raj hooks in raj-dev, as MCP tools. Commands run in the raj-dev
# container, never on the host, and only as hooks: the human authors each one
# (`rajdev-sh raj hook add ...`), and an agent names a hook and the parameter
# values its author declared, never a command line. The server reaches the
# editor over TCP, so it is an agent there and only `--agent` hooks run.
#
# Build in raj-dev: ruby rmcp_dsl/exe/rmcp_dsl build exec-mcp-server/exec.rmcp.rb
# (the exec-mcp-build hook). `rajdev` starts the binary next to the daemon.
server "exec", version: "0.1.0", instructions: "Runs the raj hooks the user authored in the raj-dev container: builds, tests and git reads for the projects. list_hooks shows what exists and the parameters each takes; run_hook runs one by name and returns its output if it finishes within its wait, or a job number to collect with hook_result, since a full check takes minutes; list_runs shows the recent runs and the tail of one. A hook that is not agent-callable is refused by the editor: ask the user rather than looking for another way to run the command." do
  # The runner lives in hand-written Rust beside this file: it starts `raj hook`
  # with a TCP address and the token, a timeout per call, and the environment
  # cleared, which cmd_fn's fixed 10 s timeout and PATH-only environment cannot.
  rust_file "rust/raj_hook.rs", as: :raj_hook
  rust_fn :raj_hook, args: [:string, :string, :string, :i64], returns: :string, from: :raj_hook
  # A run in the background, so a test suite outlives no client's tool timeout:
  # start it and wait a while, then collect it by job number.
  rust_fn :raj_hook_start, args: [:string, :string, :string, :i64], returns: :string, from: :raj_hook
  rust_fn :raj_hook_job, args: [:i64, :i64], returns: :string, from: :raj_hook

  setting :raj_addr, env: "RAJ_EXEC_ADDR", default: "tcp://127.0.0.1:7391", description: "the editor's TCP control address; must be tcp:// so every hook call is an agent's"
  setting :raj_token, env: "RAJ_CONTROL_TOKEN", secret: true, description: "the editor's control token"
  setting :mcp_token, env: "MCP_TOKEN", secret: true, description: "bearer token every request to the HTTP transport must carry"

  # The result of one `raj hook` call (rust(:raj_hook, addr, token, args, timeout),
  # args one argument per line; settings are read in tool bodies only). A refusal
  # or a nonzero exit is an error result carrying the output.
  helper :checked, args: [:string], returns: :string do |out|
    raise out if out.start_with?("error:")
    out
  end

  params :ListParams do
    field :name, :string, optional: true, pattern: "^[A-Za-z0-9_-]+$",
          description: "one hook to show in full (its action, tree, timeout and parameters); leave out to list them all"
  end

  params :RunParams do
    field :name, :string, pattern: "^[A-Za-z0-9_-]+$", description: "the hook to run, as list_hooks names it"
    field :params, :string_list, default: [],
          description: "the hook's declared parameters as NAME=value entries, such as [\"project=raj\"]; list_hooks shows which it takes"
    field :wait, :i64, default: 20, min: 0, max: 600,
          description: "seconds to wait for the run; one that takes longer keeps going and comes back as a job number for hook_result"
  end

  params :ResultParams do
    field :job, :i64, min: 1, description: "the job number run_hook returned"
    field :wait, :i64, default: 20, min: 0, max: 600, description: "seconds to wait for the run to finish before saying it is still running"
  end

  params :RunsParams do
    field :run_id, :i64, optional: true, min: 1, description: "one run to show the log of; leave out for the runs in flight and the last 100"
    field :tail, :i64, optional: true, min: 1, max: 10000, description: "with run_id: how many lines from the end of its log"
  end

  tool :list_hooks, params: :ListParams, title: "List hooks",
       description: "The hooks in raj-dev: name, tree, whether an agent may run it, timeout and declared parameters. With name, one hook in full",
       read_only: true, open_world: false do
    body do |name|
      n = name || ""
      args = if n == "" then "list" else "show\n#{n}" end
      checked(rust(:raj_hook, setting(:raj_addr), setting(:raj_token), args, 30))
    end
  end

  tool :run_hook, params: :RunParams, title: "Run a hook",
       description: "Run one hook by name in raj-dev. Waits up to `wait` seconds and returns its output and exit status if it finished; a longer run (builds and test suites take minutes) keeps going and comes back as \"running: job N\", collected with hook_result. A nonzero exit or a refusal (not agent-callable, workspace not ready, cooldown) is an error result with the output",
       destructive: false, idempotent: false, open_world: false do
    body do |name, params, wait|
      raise "a parameter is NAME=value on one line" if params.any? { |p| p.include?("\n") || !p.include?("=") }
      extra = params.join("\n")
      checked(rust(:raj_hook_start, setting(:raj_addr), setting(:raj_token), "run\n#{name}\n#{extra}", wait))
    end
  end

  tool :hook_result, params: :ResultParams, title: "Hook run result",
       description: "Wait up to `wait` seconds for a run that run_hook returned as a job, then its output and exit status, or \"running\" again if it has not finished. Jobs last while the exec server runs",
       read_only: true, open_world: false do
    body do |job, wait|
      checked(rust(:raj_hook_job, job, wait))
    end
  end

  tool :list_runs, params: :RunsParams, title: "Hook runs",
       description: "The hook runs in flight and the last 100 with their exit status; with run_id, the tail of that run's log",
       read_only: true, open_world: false do
    body do |run_id, tail|
      id = run_id || 0
      if id == 0
        ps = checked(rust(:raj_hook, setting(:raj_addr), setting(:raj_token), "ps", 30))
        log = checked(rust(:raj_hook, setting(:raj_addr), setting(:raj_token), "log", 30))
        "In flight:\n#{ps}\n\nRecent:\n#{log}"
      else
        checked(rust(:raj_hook, setting(:raj_addr), setting(:raj_token), "log\n--show\n#{id}\n--tail\n#{tail || 200}", 30))
      end
    end
  end

  transport :http, port: 8788, bind: "0.0.0.0", auth_setting: :mcp_token,
            allowed_hosts: ["host.docker.internal", "127.0.0.1", "localhost"]
end

