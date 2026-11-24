/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {renderToStaticMarkup} from 'react-dom/server';
import {renderSimpleMarkdown} from '../SimpleMarkdown';

jest.mock('../SimpleMarkdown.css', () => ({}));

function render(text: string): string {
  return renderToStaticMarkup(<>{renderSimpleMarkdown(text)}</>);
}

describe('renderSimpleMarkdown', () => {
  it('keeps underscores inside identifiers', () => {
    const text =
      'Passing include_tgif=False and include_elastic_0=False removes the elastic_0 base template.';
    expect(render(text)).toBe(`<span>${text}</span>`);
  });

  it('renders underscore emphasis at word boundaries', () => {
    expect(render('This is _important_, and (_so is this_).')).toBe(
      '<span>This is <em>important</em>, and (<em>so is this</em>).</span>',
    );
  });
});
