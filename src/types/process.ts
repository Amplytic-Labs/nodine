/**
 * Options that control spawning a process in the container.
 *
 * @public
 */
export interface SpawnOptions {
  /**
   * Current working directory for the process, relative to the container's
   * working directory. Defaults to the container working directory.
   */
  cwd?: string;

  /** Environment variables to set for the process. */
  env?: Record<string, string | number | boolean>;

  /**
   * When set to `false`, no terminal output is sent back and the
   * {@link ContainerProcess.output} stream never produces chunks.
   *
   * @defaultValue `true`
   */
  output?: boolean;

  /**
   * Terminal-attached stdio: the process and ALL its descendants write to
   * the container's shared terminal stream (see {@link Container.onOutput})
   * instead of per-process pipes — interactive shell semantics. Required
   * when spawning a shell (`/bin/sh -c …`) whose children's output must be
   * visible; with the default piped mode the runtime currently drops
   * inherit-mode grandchildren from the pipe (known runtime limitation).
   * When `stdio: 'inherit'`, {@link ContainerProcess.output} produces no
   * chunks — read via {@link Container.onOutput}.
   *
   * @defaultValue `'pipe'`
   */
  stdio?: 'pipe' | 'inherit';

  /**
   * The size of the attached terminal, for processes that behave differently
   * when connected to a TTY.
   */
  terminal?: TerminalDimensions;
}

/** Terminal dimensions in character cells. */
export interface TerminalDimensions {
  /** Number of columns. */
  cols: number;
  /** Number of rows. */
  rows: number;
}

/**
 * A running process spawned in a container.
 *
 * @public
 */
export interface ContainerProcess {
  /**
   * A promise for the exit code of the process. Resolves when the process
   * exits; never rejects.
   */
  exit: Promise<number>;

  /**
   * An input stream for the attached pseudoterminal device. Write strings
   * (typically user keystrokes) to feed the process's standard input.
   */
  input: WritableStream<string>;

  /**
   * A stream that receives all terminal output — standard output and
   * standard error emitted by the process and its descendants. Can be
   * disabled at spawn time with `output: false`.
   */
  output: ReadableStream<string>;

  /** Kills the process. */
  kill(): void;

  /**
   * Sends SIGINT to the process tree (Ctrl+C semantics): foreground
   * processes get a chance to exit cleanly. Not fatal-by-design; a
   * process that ignores SIGINT keeps running.
   */
  interrupt(): void;

  /** Resizes the attached terminal. */
  resize(dimensions: TerminalDimensions): void;
}
