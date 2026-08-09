#set page(width: 8.5in, height: 11in, margin: 0.75in)
#set text(font: "Helvetica", size: 18pt)

= Synthetic PDF fixture

Page 1 contains generated text and shapes for pdfterm smoke testing.

Follow the #link(<synthetic-reference>)[synthetic reference] or copy the
#link("https://example.invalid/paper")[synthetic external link].

#rect(width: 100%, height: 3in, fill: gradient.linear(blue, purple))

#pagebreak()

= Synthetic second page

#for value in range(1, 41) [
  #value. This line is synthetic benchmark content.\
]

#pagebreak()

= Synthetic third page <synthetic-reference>

#circle(radius: 2in, fill: orange, stroke: 4pt + black)
