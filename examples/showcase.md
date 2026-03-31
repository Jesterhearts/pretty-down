# pretty-down Feature Showcase

This document demonstrates all supported rendering features.

## Text Styling

**Bold text**, *italic text*, and ***bold italic*** together.

~~Strikethrough~~ text and `inline code` too.

## Links

[A hyperlink](https://example.com) using OSC 8 — clickable in supported terminals.

## Lists

### Unordered

- First item
- Second item with **bold**
- Third item with *italic*
  - Nested item
  - Another nested item

### Ordered

1. First
2. Second
3. Third

## Images

Images are loaded and rendered inline as sixel graphics:

![Mountain landscape](example_image.png)

### Remote Images

Images can also be loaded from URLs (including SVGs):

![Ferris](https://rustacean.net/assets/rustacean-flat-noshadow.png) ![Rust logo](https://www.rust-lang.org/logos/rust-logo-512x512.png)

## Gifs

Gifs can also be used:

![Terran Planet](example_gif.gif) ![Blackhole](example_gif_2.gif)

## Videos

![Nature](example_video.webm)

## Mermaid Diagrams

```mermaid
flowchart LR
    A[Start] --> B{Decision}
    B -->|Yes| C[OK]
    B -->|No| D[Cancel]
    C --> E[End]
    D --> E
```

## Code Blocks

Syntax highlighting is applied automatically when a language is specified.

### Rust

```rust
fn main() {
    let names = vec!["Alice", "Bob", "Charlie"];
    for name in &names {
        println!("Hello, {name}!");
    }
}
```

### Python

```python
def fibonacci(n: int) -> list[int]:
    """Generate the first n Fibonacci numbers."""
    fib = [0, 1]
    for _ in range(2, n):
        fib.append(fib[-1] + fib[-2])
    return fib[:n]

print(fibonacci(10))
```

### JavaScript

```javascript
async function fetchUsers() {
  const response = await fetch('/api/users');
  const users = await response.json();
  return users.filter(u => u.active).map(u => u.name);
}
```

### Go

```go
package main

import "fmt"

func main() {
    ch := make(chan string, 2)
    ch <- "hello"
    ch <- "world"
    fmt.Println(<-ch)
    fmt.Println(<-ch)
}
```

### Shell

```bash
#!/bin/bash
for file in *.md; do
    echo "Rendering: $file"
    pretty-down "$file" --no-pager > "${file%.md}.txt"
done
```

### Plain (no language)

```
This block has no language tag.
It uses the theme's code_block style without syntax highlighting.
```

## Blockquotes

> This is a blockquote with *italic* and **bold** inside.

## Horizontal Rule

---

## Tables

| Feature       | Status    | Notes                  |
|---------------|:---------:|------------------------|
| Headings      | Done      | Sixel rendered         |
| **Bold**      | Done      | ANSI escape            |
| *Italic*      | Done      | ANSI escape            |
| `Code`        | Done      | Dimmed                 |
| Tables        | Done      | custom tables          |
| Images        | Done      | Background encoding    |

### Images In Tables
| Text   | Image                      | Video                         |
| ---    | ---                        | ---                           |
| Planet | ![planet](example_gif.gif) | ![nature](example_video.webm) |

## HTML Elements

### Inline Styling

<b>HTML bold</b> and <i>HTML italic</i> and <em>emphasis</em> and <strong>strong</strong>.

<u>Underlined text</u> and <mark>highlighted text</mark>.

<del>Deleted</del> and <s>strikethrough</s>.

<kbd>Ctrl</kbd>+<kbd>C</kbd> for keyboard shortcuts.

<var>x</var> = <var>y</var> + 1 for variables, and <cite>A Citation</cite>.

<sup>superscript</sup> and <sub>subscript</sub>.

### Links

<a href="https://example.com">An HTML link</a>.

### CSS Inline Styles

<span style="color: red">Red text</span> and <span style="color: #00ff00">green hex text</span>.

<span style="background-color: navy; color: white">White on navy background</span>.

<span style="font-weight: bold; font-style: italic">Bold italic via CSS</span>.

<span style="text-decoration: underline">CSS underline</span> and <span style="text-decoration:
line-through">CSS strikethrough</span>.

<span style="color: coral; font-weight: bold">Coral bold</span>,
<span style="color: gold">gold</span>,
<span style="color: crimson">crimson</span>,
<span style="color: teal">teal</span>,
<span style="color: violet">violet</span>.

### Block HTML

<h3>An HTML Heading</h3>

<p>A paragraph from HTML.</p>

<pre><code>Code inside pre tags.</code></pre>

<hr>

<blockquote>An HTML blockquote.</blockquote>

## Details (Collapsible)

<details>
<summary>Click to expand this section</summary>

This content is hidden by default in the pager.

- You can have **lists** inside details
- And other *markdown* content

```
Even code blocks work here.
```

</details>

<details>
<summary>Another collapsible section</summary>

More hidden content with <span style="color: orange">styled HTML</span> inside.

</details>

## Footnotes

Here is a sentence with a footnote[^1] and another[^note].

[^1]: This is the first footnote. Click to jump back to the reference.
[^note]: Footnotes can have any label, not just numbers.

## Mixed Content

A paragraph with **bold**, *italic*, `code`, ~~strikethrough~~,
<u>underline</u>, <mark>highlight</mark>, and
<span style="color: skyblue">colored text</span> all in one line.

> A blockquote containing a [link](https://example.com) and **bold text**.

---

*End of showcase.*
