import { Fragment } from "react";

const START = "\u0001";
const END = "\u0002";

export function SnippetText({ value }: { value: string }) {
  if (!value.includes(START)) {
    return <>{value}</>;
  }
  const chunks = value.split(START);
  return (
    <>
      {chunks.map((chunk, index) => {
        if (index === 0) {
          return <Fragment key={index}>{chunk}</Fragment>;
        }
        const end = chunk.indexOf(END);
        if (end === -1) {
          return <Fragment key={index}>{chunk}</Fragment>;
        }
        return (
          <Fragment key={index}>
            <mark>{chunk.slice(0, end)}</mark>
            {chunk.slice(end + END.length)}
          </Fragment>
        );
      })}
    </>
  );
}

function queryTokens(query: string) {
  const found = query.match(/[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*/g) ?? [];
  return [...new Set(found.filter((token) => token.length >= 2 || /\d/.test(token)))];
}

function escapeRegExp(value: string) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function HighlightedText({ text, query }: { text: string; query: string }) {
  const tokens = queryTokens(query);
  if (!text || tokens.length === 0) {
    return <>{text}</>;
  }
  const pattern = new RegExp(`(${tokens.map(escapeRegExp).join("|")})`, "gi");
  const parts = text.split(pattern);
  return (
    <>
      {parts.map((part, index) =>
        tokens.some((token) => token.toLowerCase() === part.toLowerCase()) ? (
          <mark key={index}>{part}</mark>
        ) : (
          <Fragment key={index}>{part}</Fragment>
        ),
      )}
    </>
  );
}
