/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import React from 'react';
import './SimpleMarkdown.css';

/**
 * Simple markdown renderer for webview messages.
 * Handles common patterns: links, bold, italic, inline code, and code blocks.
 */
export function renderSimpleMarkdown(text: string): React.ReactNode {
  // Split by code blocks first (```...```)
  const codeBlockRegex = /```[\s\S]*?```/g;
  const parts: Array<{type: 'text' | 'codeblock'; content: string}> = [];
  let lastIndex = 0;
  let match;

  while ((match = codeBlockRegex.exec(text)) !== null) {
    if (match.index > lastIndex) {
      parts.push({type: 'text', content: text.slice(lastIndex, match.index)});
    }
    parts.push({type: 'codeblock', content: match[0].slice(3, -3).trim()});
    lastIndex = match.index + match[0].length;
  }
  if (lastIndex < text.length) {
    parts.push({type: 'text', content: text.slice(lastIndex)});
  }

  return parts.map((part, i) => {
    if (part.type === 'codeblock') {
      return (
        <pre key={i} className="simple-markdown-codeblock">
          <code>{part.content}</code>
        </pre>
      );
    }
    return <span key={i}>{renderInlineMarkdown(part.content)}</span>;
  });
}

function renderInlineMarkdown(text: string): React.ReactNode {
  // Combined regex for markdown patterns
  // Order matters: links first, then other patterns
  // Note: \s* allows whitespace (including newlines) between ] and ( for links
  // Like CommonMark, _ only opens or closes emphasis outside a word, so snake_case stays intact
  const inlineRegex =
    /(\[([^\]]+)\]\s*\(([^)]+)\))|(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*]+\*)|((?<!\w)_[^_]+_(?!\w))/g;

  const result: React.ReactNode[] = [];
  let lastIndex = 0;
  let match;

  while ((match = inlineRegex.exec(text)) !== null) {
    // Add text before the match
    if (match.index > lastIndex) {
      result.push(text.slice(lastIndex, match.index));
    }

    if (match[1]) {
      // Link: [text](url)
      result.push(
        <a
          key={`link-${match.index}`}
          href={match[3].trim()}
          target="_blank"
          rel="noopener noreferrer"
          className="simple-markdown-link">
          {match[2]}
        </a>,
      );
    } else if (match[4]) {
      // Inline code: `code`
      result.push(
        <code key={`code-${match.index}`} className="simple-markdown-inline-code">
          {match[4].slice(1, -1)}
        </code>,
      );
    } else if (match[5]) {
      // Bold: **text** - recursively process content for nested patterns
      const innerContent = match[5].slice(2, -2);
      result.push(
        <strong key={`bold-${match.index}`}>{renderInlineMarkdown(innerContent)}</strong>,
      );
    } else if (match[6]) {
      // Italic: *text* - recursively process content for nested patterns
      const innerContent = match[6].slice(1, -1);
      result.push(<em key={`italic-${match.index}`}>{renderInlineMarkdown(innerContent)}</em>);
    } else if (match[7]) {
      // Italic (underscore): _text_ - recursively process content for nested patterns
      const innerContent = match[7].slice(1, -1);
      result.push(<em key={`italic2-${match.index}`}>{renderInlineMarkdown(innerContent)}</em>);
    }

    lastIndex = match.index + match[0].length;
  }

  // Add remaining text
  if (lastIndex < text.length) {
    result.push(text.slice(lastIndex));
  }

  return result.length > 0 ? result : text;
}
