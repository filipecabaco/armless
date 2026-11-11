#!/usr/bin/env elixir
# What each snowglobe example asks of a kernel: its own command, run as its image's user under
# `strace -f` in a pseudo-terminal, with its exercises (the label snowglobe.exercise) typed in. For
# each image: whether its programs are dynamically linked, how many processes and programs it
# starts, the sockets, files and terminal calls it makes, and every syscall, against what armless's
# user mode answers today. The corpus that says what user mode needs next.
#
#   elixir user/corpus/survey.exs --examples ../snowglobe/examples [--only go,python] [--out DIR]
#
# Needs Docker (arm64 images; on an x86 host, emulation). The images get `strace` and `script`
# added with apk in the run, so the examples must be Alpine (they all are).

Mix.install([{:table_rex, "~> 4.1"}])

defmodule Survey do
  # The syscalls armless-user answers for real (user/src/lib.rs), by strace's names. openat,
  # newfstatat and ioctl are only stubs there (ENOENT, ENOTTY), so they count as missing.
  @answered ~w(read write writev close lseek fcntl eventfd2 epoll_create1 epoll_ctl epoll_pwait
    brk mmap munmap mprotect msync mlock madvise clone futex set_tid_address set_robust_list prctl
    membarrier rseq sched_yield sched_getaffinity sched_setaffinity setpriority exit exit_group gettid
    getpid getppid getuid geteuid getgid getegid rt_sigaction rt_sigprocmask sigaltstack kill tkill
    tgkill clock_gettime clock_getres nanosleep clock_nanosleep getrandom prlimit64 uname fstat)

  def run(argv) do
    {o, _} = OptionParser.parse!(argv, strict: [examples: :string, only: :string, out: :string, timeout: :integer])
    root = Path.expand(o[:examples] || "../snowglobe/examples")
    out = Path.expand(o[:out] || "user/corpus/results")
    File.mkdir_p!(out)
    only = if o[:only], do: String.split(o[:only], ","), else: nil

    names =
      for d <- File.ls!(root) |> Enum.sort(),
          File.exists?(Path.join([root, d, "Dockerfile"])),
          d != "supa-wasm",
          only == nil or d in only,
          do: d

    rows =
      for name <- names do
        IO.puts(:stderr, "== #{name}")
        case survey(root, name, out, o[:timeout] || 90) do
          {:ok, r} ->
            File.write!(Path.join(out, "#{name}.json"), JSON.encode!(r))
            row(name, r)

          {:error, why} ->
            IO.puts(:stderr, "   #{why}")
            [name, "error: #{String.slice(why, 0, 60)}", "", "", "", "", "", ""]
        end
      end

    IO.puts(TableRex.quick_render!(rows, ["example", "dynamic", "procs", "programs", "sockets",
      "files", "tty ioctls", "syscalls (missing in user mode)"]))
  end

  defp row(name, r) do
    missing = r["syscalls"] |> Map.keys() |> Enum.reject(&(&1 in @answered)) |> Enum.sort()
    [name, if(r["dynamic"], do: "yes", else: "static"), r["processes"], length(r["programs"]),
     r["sockets"] |> Map.keys() |> Enum.join(" "), r["files_opened"],
     r["tty_ioctls"] |> Enum.join(" "),
     "#{map_size(r["syscalls"])} (#{length(missing)}): #{Enum.join(Enum.take(missing, 14), " ")}#{if length(missing) > 14, do: " ..."}"]
  end

  defp survey(root, name, out, timeout) do
    tag = "sgx-#{name}"
    with {_, 0} <- System.cmd("docker", ["build", "-q", "--platform", "linux/arm64", "-t", tag, Path.join(root, name)], stderr_to_stdout: true),
         {json, 0} <- System.cmd("docker", ["image", "inspect", tag, "--format", "{{json .Config}}"]) do
      config = JSON.decode!(json)
      argv = (config["Entrypoint"] || []) ++ (config["Cmd"] || [])
      user = config["User"] |> to_string() |> then(&if(&1 == "", do: "root", else: &1))
      exercises = config["Labels"]["snowglobe.exercise"] |> then(&if(&1, do: JSON.decode!(&1), else: []))
      dir = Path.join(out, "#{name}.run")
      File.rm_rf!(dir)
      File.mkdir_p!(dir)
      File.write!(Path.join(dir, "input"), Enum.join(exercises, "\n") <> "\n")
      File.write!(Path.join(dir, "cmd"), Enum.map_join(argv, " ", &shell_quote/1))
      File.write!(Path.join(dir, "trace.sh"), trace_script(user, config["WorkingDir"]))
      File.chmod!(Path.join(dir, "trace.sh"), 0o755)
      System.cmd("docker", ["run", "--rm", "--platform", "linux/arm64", "--user", "0",
        "--cap-add", "SYS_PTRACE", "--security-opt", "seccomp=unconfined",
        "-v", "#{dir}:/x", "--entrypoint", "/x/trace.sh", tag, to_string(timeout)], stderr_to_stdout: true)
      case File.read(Path.join(dir, "strace.log")) do
        {:ok, log} -> {:ok, analyse(log) |> Map.merge(%{"argv" => argv, "user" => user, "exercises" => length(exercises)})}
        _ -> {:error, File.read(Path.join(dir, "trace.out")) |> elem(1) |> to_string() |> String.slice(-300, 300)}
      end
    else
      {out, code} -> {:error, "exit #{code}: #{String.slice(to_string(out), -300, 300)}"}
    end
  end

  defp shell_quote(s), do: "'" <> String.replace(s, "'", "'\\''") <> "'"

  # Inside the container: strace and script added, the command run as the image's user in a
  # pseudo-terminal, the exercises typed in a line every 2 s once it has had 8 s to start
  defp trace_script(user, workdir) do
    """
    #!/bin/sh
    apk add -q --no-cache strace util-linux-misc >/x/trace.out 2>&1 || exit 1
    cd #{shell_quote(workdir || "/")}
    { sleep 8; while IFS= read -r l; do printf '%s\\n' "$l"; sleep 2; done < /x/input; sleep 4; printf 'exit\\n'; sleep 1; } |
      timeout "$1" script -qfec "strace -f -qq -s 160 -o /x/strace.log -u #{user} $(cat /x/cmd)" /dev/null >>/x/trace.out 2>&1
    exit 0
    """
  end

  defp analyse(log) do
    lines = String.split(log, "\n", trim: true)
    calls =
      for l <- lines, [_, pid, sys] <- [Regex.run(~r/^(\d+)\s+([a-z0-9_]+)\(/, l)], do: {pid, sys, l}

    syscalls = Enum.frequencies_by(calls, &elem(&1, 1))
    programs =
      for {_, "execve", l} <- calls, [_, p] <- [Regex.run(~r/execve\("([^"]+)"/, l)], uniq: true, do: p

    sockets =calls |> Enum.filter(&(elem(&1, 1) == "socket")) |> Enum.flat_map(&(Regex.run(~r/socket\((AF_[A-Z0-9]+)/, elem(&1, 2), capture: :all_but_first) || [])) |> Enum.frequencies()
    opens = for {_, s, l} <- calls, s in ~w(openat open), [_, p] <- [Regex.run(~r/"([^"]+)"/, l)], do: p

    %{
      "syscalls" => syscalls,
      "processes" => calls |> Enum.map(&elem(&1, 0)) |> Enum.uniq() |> length(),
      "programs" => programs,
      "dynamic" => Enum.any?(opens, &String.match?(&1, ~r{\.so(\.\d+)*$})),
      "shared_libs" => opens |> Enum.filter(&String.match?(&1, ~r{\.so(\.\d+)*$})) |> Enum.uniq() |> length(),
      "sockets" => sockets,
      "files_opened" => opens |> Enum.uniq() |> length(),
      "tty_ioctls" =>
        for({_, "ioctl", l} <- calls, [_, r] <- [Regex.run(~r/ioctl\(\d+, ([A-Z][A-Z0-9_]+)/, l)], uniq: true, do: r) |> Enum.sort(),
      "forks" => Enum.count(calls, fn {_, s, l} -> s in ~w(fork vfork) or (s in ~w(clone clone3) and not String.contains?(l, "CLONE_THREAD")) end),
      "threads" => Enum.count(calls, fn {_, s, l} -> s in ~w(clone clone3) and String.contains?(l, "CLONE_THREAD") end),
      "fault_signals" => Enum.count(lines, &String.contains?(&1, "--- SIGSEGV"))
    }
  end
end

Survey.run(System.argv())
