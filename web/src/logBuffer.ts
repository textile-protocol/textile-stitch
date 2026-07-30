// A bounded tail of streamed lines, shared by everything that follows output.
//
// Both the log tail and the one-shot runner read a stream that has no natural
// end: a bot at debug level emits thousands of lines a minute, and a dry run
// loops until it's told to stop. Without a ceiling the tab's memory grows until
// the browser kills it, taking the tail the operator was watching with it.

export const MAX_LINES = 2000

/** Append a line, dropping from the front once the ceiling is reached. */
export function appendLine<T>(lines: T[], line: T): T[] {
  return lines.length >= MAX_LINES
    ? [...lines.slice(lines.length - MAX_LINES + 1), line]
    : [...lines, line]
}
