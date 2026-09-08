"""Generate demo fixtures for an end-to-end pipeline run:
   - inspection.pdf : hand-built minimal 1-page PDF with inspection text
   - fee_card.pdf   : a payment-services rate card whose columns are aligned with
                      whitespace only (no ruled lines) — the case that broke the
                      table path. The answer to "what fee on a 1000 credit-card
                      payment" (2.00%) is one row of it.
   - valve.png      : synthetic 'equipment photo' (shapes + label) for the VLM
"""
import os, zlib, struct, textwrap
from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), "fixtures")
os.makedirs(OUT, exist_ok=True)


def write_text_pdf(path, lines, font_size=11, leading=14, x=54, y=760):
    """Minimal one-page text PDF. `lines` is a list of strings; columns should be
    pre-aligned with spaces by the caller (Helvetica is not monospace, but
    pdfplumber's layout mode reconstructs the grid from x-positions well enough)."""

    def esc(s):
        return s.replace("\\", r"\\").replace("(", r"\(").replace(")", r"\)")

    content = f"BT /F1 {font_size} Tf {x} {y} Td {leading} TL\n"
    for ln in lines:
        content += f"({esc(ln)}) Tj T*\n"
    content += "ET"
    cb = content.encode("latin-1")

    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length %d >>\nstream\n" % len(cb) + cb + b"\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    pdf = b"%PDF-1.4\n"
    offsets = []
    for i, body in enumerate(objs, start=1):
        offsets.append(len(pdf))
        pdf += b"%d 0 obj\n" % i + body + b"\nendobj\n"
    xref_pos = len(pdf)
    pdf += b"xref\n0 %d\n" % (len(objs) + 1)
    pdf += b"0000000000 65535 f \n"
    for off in offsets:
        pdf += b"%010d 00000 n \n" % off
    pdf += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF" % (
        len(objs) + 1,
        xref_pos,
    )
    with open(path, "wb") as f:
        f.write(pdf)


# ---------- borderless fee card ----------
fee_lines = [
    "PayU India  -  Merchant Discount Rate (MDR) schedule  -  effective 2026-04-01",
    "",
    "Payment mode              MDR (%)     Fixed fee (INR)     GST on fee",
    "Credit card (domestic)    2.00        0.00               18%",
    "Debit card up to 2000     0.40        0.00               18%",
    "Debit card above 2000     0.90        0.00               18%",
    "UPI                       0.00        0.00               NA",
    "Net banking               1.20        3.00               18%",
    "Wallet                    1.50        0.00               18%",
    "",
    "MDR is charged on the transaction amount and deducted before settlement.",
    "Example: a 1000 INR domestic credit-card payment is charged 2.00% = 20.00 INR,",
    "plus 18% GST on the fee = 3.60 INR.",
]
write_text_pdf(os.path.join(OUT, "fee_card.pdf"), fee_lines)

# ---------- minimal PDF ----------
lines = [
    "MRPL Unit-2  -  Pump P-101 External Inspection Sheet",
    "Date: 2026-09-04    Inspector: T. Rao",
    "",
    "1. Mechanical seal: steady clear drip, approx 12 drops/min.",
    "2. Casing drain small-bore line: moderate scaling, pitting about 1.5 mm deep.",
    "3. Bearing housing temperature: 71 C (ambient 33 C).",
    "4. Overall vibration: 5.2 mm/s RMS.",
    "5. Coating loss with flaky rust over a 90 mm patch near the discharge flange.",
    "",
    "Recommendation: schedule seal replacement; UT thickness check on drain line.",
]

def pdf_escape(s):
    return s.replace("\\", r"\\").replace("(", r"\(").replace(")", r"\)")

content = "BT /F1 11 Tf 54 760 Td 14 TL\n"
for ln in lines:
    content += f"({pdf_escape(ln)}) Tj T*\n"
content += "ET"
content_b = content.encode("latin-1")

objs = []
objs.append(b"<< /Type /Catalog /Pages 2 0 R >>")
objs.append(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
objs.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>")
objs.append(b"<< /Length %d >>\nstream\n" % len(content_b) + content_b + b"\nendstream")
objs.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")

pdf = b"%PDF-1.4\n"
offsets = []
for i, body in enumerate(objs, start=1):
    offsets.append(len(pdf))
    pdf += b"%d 0 obj\n" % i + body + b"\nendobj\n"
xref_pos = len(pdf)
pdf += b"xref\n0 %d\n" % (len(objs) + 1)
pdf += b"0000000000 65535 f \n"
for off in offsets:
    pdf += b"%010d 00000 n \n" % off
pdf += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF" % (len(objs) + 1, xref_pos)

with open(os.path.join(OUT, "inspection.pdf"), "wb") as f:
    f.write(pdf)

# ---------- synthetic equipment image ----------
img = Image.new("RGB", (900, 600), (140, 145, 150))
d = ImageDraw.Draw(img)
d.rectangle([120, 200, 780, 380], fill=(90, 92, 96), outline=(40, 40, 40), width=4)   # pump body
d.ellipse([300, 150, 500, 430], fill=(70, 72, 78), outline=(30, 30, 30), width=4)      # flange
for x in range(140, 760, 40):                                                          # rust streaks
    d.line([x, 380, x + 12, 470], fill=(120, 70, 35), width=6)
d.ellipse([600, 300, 690, 360], fill=(110, 60, 30))                                    # corrosion patch
d.text((130, 60), "P-101  DISCHARGE", fill=(20, 20, 20))
d.text((610, 250), "corrosion", fill=(30, 10, 5))
img.save(os.path.join(OUT, "valve.png"))

print("wrote:", os.listdir(OUT))
