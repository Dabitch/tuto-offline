#!/usr/bin/env python3
"""Write the fake papers in fixtures/samples. Nothing here is a real document."""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

OUT = Path(__file__).resolve().parent / "samples"
SANS = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
MONO = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"


def font(path: str, size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(path, size)


def wrap(draw: ImageDraw.ImageDraw, text: str, face: ImageFont.FreeTypeFont, max_width: int) -> list[str]:
    lines: list[str] = []
    for paragraph in text.split("\n"):
        if not paragraph.strip():
            lines.append("")
            continue
        current = ""
        for word in paragraph.split():
            trial = word if not current else f"{current} {word}"
            if draw.textlength(trial, font=face) <= max_width:
                current = trial
            else:
                if current:
                    lines.append(current)
                current = word
        if current:
            lines.append(current)
    return lines


def render_doc(name: str, title: str, body: str, accent: tuple[int, int, int] = (140, 58, 47)) -> None:
    width, height = 1200, 1600
    image = Image.new("RGB", (width, height), (250, 246, 236))
    draw = ImageDraw.Draw(image)
    draw.rectangle([36, 36, width - 36, height - 36], outline=(210, 196, 170), width=3)
    draw.rectangle([48, 48, width - 48, 168], fill=accent)
    draw.text((80, 78), title, font=font(BOLD, 54), fill=(255, 248, 236))
    body_font = font(SANS, 40)
    y = 220
    for line in wrap(draw, body, body_font, width - 180):
        draw.text((90, y), line, font=body_font, fill=(28, 25, 21))
        y += 58 if line else 28
    image.save(OUT / name, "PNG")


def render_receipt(name: str, store: str, lines: list[tuple[str, str]], total: str, extra: str, crooked: bool = False) -> None:
    width, height = 980, 1400
    image = Image.new("RGB", (width, height), (255, 252, 245))
    draw = ImageDraw.Draw(image)
    draw.rectangle([70, 50, width - 70, height - 50], outline=(40, 40, 40), width=2)
    draw.text((width / 2, 100), store, font=font(BOLD, 42), fill=(20, 20, 20), anchor="ma")
    draw.text((width / 2, 170), "18 Birch Lane", font=font(SANS, 28), fill=(60, 60, 60), anchor="ma")
    draw.line([110, 250, width - 110, 250], fill=(40, 40, 40), width=2)
    y = 280
    mono = font(MONO, 32)
    for label, amount in lines:
        draw.text((120, y), label, font=mono, fill=(20, 20, 20))
        draw.text((width - 120, y), amount, font=mono, fill=(20, 20, 20), anchor="ra")
        y += 56
    y += 10
    draw.line([110, y, width - 110, y], fill=(40, 40, 40), width=3)
    y += 30
    draw.text((120, y), "TOTAL", font=font(BOLD, 40), fill=(20, 20, 20))
    draw.text((width - 120, y), total, font=font(BOLD, 40), fill=(20, 20, 20), anchor="ra")
    draw.text((120, y + 80), extra, font=font(SANS, 28), fill=(50, 50, 50))
    if crooked:
        image = image.rotate(8, expand=True, fillcolor=(232, 224, 208), resample=Image.Resampling.BICUBIC)
        framed = Image.new("RGB", (image.width + 80, image.height + 120), (48, 52, 48))
        framed.paste(image, (40, 60))
        framed.save(OUT / name, "JPEG", quality=86)
        return
    image.save(OUT / name, "PNG")


def escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def write_text_pdf(path: Path, pages: list[list[str]]) -> None:
    page_ids: list[int] = []
    content_ids: list[int] = []
    streams: list[bytes] = []
    next_id = 3
    for lines in pages:
        page_ids.append(next_id)
        content_ids.append(next_id + 1)
        next_id += 2
        parts = ["BT", "/F1 14 Tf", "72 740 Td", "20 TL"]
        for index, line in enumerate(lines):
            if index:
                parts.append("T*")
            parts.append(f"({escape(line)}) Tj")
        parts.append("ET")
        streams.append("\n".join(parts).encode("latin-1"))
    font_id = next_id

    def obj(number: int, body: bytes) -> bytes:
        return f"{number} 0 obj\n".encode() + body + b"\nendobj\n"

    blobs = [obj(1, b"<< /Type /Catalog /Pages 2 0 R >>")]
    kids = " ".join(f"{page_id} 0 R" for page_id in page_ids)
    blobs.append(obj(2, f"<< /Type /Pages /Kids [{kids}] /Count {len(page_ids)} >>".encode()))
    for page_id, content_id, stream in zip(page_ids, content_ids, streams):
        blobs.append(
            obj(
                page_id,
                f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {content_id} 0 R /Resources << /Font << /F1 {font_id} 0 R >> >> >>".encode(),
            )
        )
        blobs.append(obj(content_id, f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream"))
    blobs.append(obj(font_id, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"))

    header = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n"
    chunks = [header]
    offsets = [0]
    cursor = len(header)
    for blob in blobs:
        offsets.append(cursor)
        chunks.append(blob)
        cursor += len(blob)
    count = len(blobs) + 1
    xref = [f"xref\n0 {count}\n", "0000000000 65535 f \n"]
    xref.extend(f"{offset:010d} 00000 n \n" for offset in offsets[1:])
    trailer = f"trailer << /Size {count} /Root 1 0 R >>\nstartxref\n{cursor}\n%%EOF\n"
    path.write_bytes(b"".join(chunks) + "".join(xref).encode() + trailer.encode())


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    render_doc(
        "warranty-dishwasher.png",
        "WARRANTY CARD",
        "Pell and Sons Appliances\n\nProduct: Dishwasher\nModel: Horizon HD-440\nSerial: PS-184422\n\n"
        "This dishwasher warranty covers parts and labor for five years from the purchase date.\n\n"
        "The warranty includes the pump, the spray arms, and the control board.\n\n"
        "Keep this card with your receipt.\nService desk: Pell and Sons Appliances",
    )
    render_receipt(
        "receipt-cedar-hardware.png",
        "CEDAR AND CO HARDWARE",
        [("Dishwasher hose", "18.00"), ("Hose clamp", "6.50"), ("Plumber tape", "4.49"), ("Tax", "14.00")],
        "42.99",
        "Card payment. Thank you.",
    )
    render_receipt(
        "receipt-northline-market.png",
        "NORTHLINE MARKET",
        [("Oats", "4.20"), ("Milk", "3.10"), ("Apples", "5.40"), ("Tax", "0.70")],
        "13.40",
        "Member number 2201",
    )
    render_receipt(
        "receipt-harbor-cafe.png",
        "HARBOR CAFE",
        [("Oat latte", "4.75"), ("Bun", "3.25"), ("Tax", "0.64")],
        "8.64",
        "Table 4. Served by Nia.",
    )
    render_receipt(
        "receipt-harbor-cafe-phone.jpg",
        "HARBOR CAFE",
        [("Oat latte", "4.75"), ("Bun", "3.25"), ("Tax", "0.64")],
        "8.64",
        "Phone photo of the cafe receipt.",
        crooked=True,
    )
    render_receipt("receipt-pine-street-books.png", "PINE STREET BOOKS", [("Field guide", "16.00"), ("Notebook", "6.50"), ("Tax", "1.80")], "24.30", "Sold by the front desk.")
    render_receipt("receipt-metro-pharmacy.png", "METRO PHARMACY", [("Bandages", "5.25"), ("Tea", "7.40"), ("Tax", "1.01")], "13.66", "Wellness aisle.")
    render_receipt("receipt-green-garden.png", "GREEN GARDEN NURSERY", [("Basil plant", "8.00"), ("Pot", "9.50"), ("Tax", "1.40")], "18.90", "Outdoor plants.")
    render_receipt("receipt-city-parking.png", "CITY PARKING", [("Garage hour", "3.00"), ("Evening rate", "2.00")], "5.00", "Stall B12. Paid in full.")
    render_receipt("receipt-ferry-ticket.png", "HARBOR FERRY", [("Adult fare", "9.50"), ("Bicycle", "2.00")], "11.50", "Pier 2 to Birch Island.")
    render_receipt("receipt-bike-shop.png", "BIRCH BIKE WORKS", [("Tube", "8.00"), ("Labor", "15.00"), ("Tax", "1.84")], "24.84", "Rear wheel. Ready Friday.")
    render_receipt("receipt-bakery.png", "OVEN AND RYE", [("Rye loaf", "6.00"), ("Butter", "4.25"), ("Tax", "0.82")], "11.07", "Baked this morning.")
    render_doc(
        "warranty-kettle.png",
        "KETTLE WARRANTY",
        "Northline Home Goods\n\nProduct: Electric kettle\nModel: Kettle 2\n\n"
        "This kettle warranty lasts two years.\nIt covers the heating plate and the lid.\n\n"
        "It does not cover a dishwasher.\nRegister the kettle with the receipt.",
    )
    render_doc(
        "letter-library-notice.png",
        "LIBRARY NOTICE",
        "Birch Lane Library\n\nHello,\n\nThe field guide you borrowed is due on November 20.\n"
        "Renew it at the front desk or return it to the slot.\n\nThis notice is a letter, not a bill.\n\nBirch Lane Library",
        accent=(36, 74, 82),
    )
    render_doc(
        "bill-water.png",
        "WATER BILL",
        "Birch Water District\nStatement for October 2026\n\nAccount 77120\nService address: 18 Birch Lane\n\n"
        "Water use: 4 units\nAmount due: 31.15\nDue date: November 21, 2026\n\nPay at the district window.",
        accent=(32, 78, 96),
    )
    render_doc(
        "photo-folded-note.png",
        "NOTE IN THE BOX",
        "Found in the kitchen drawer.\n\nThe dishwasher hose was bought at Cedar and Co Hardware.\n"
        "The warranty card is the red one.\n\nDo not throw out the warranty.",
        accent=(90, 70, 40),
    )
    note = Image.open(OUT / "photo-folded-note.png")
    note.rotate(-6, expand=True, fillcolor=(214, 206, 190), resample=Image.Resampling.BICUBIC).save(OUT / "photo-folded-note.png", "PNG")
    write_text_pdf(
        OUT / "bill-city-light.pdf",
        [[
            "CITY LIGHT UTILITY",
            "Statement for October 2026",
            "",
            "Account 440192",
            "Service address: 18 Birch Lane",
            "",
            "Electric service",
            "Usage 642 kWh",
            "Amount due 86.40",
            "Due date November 18, 2026",
            "",
            "This is a utility bill.",
        ]],
    )
    write_text_pdf(
        OUT / "letter-mara-ellison.pdf",
        [[
            "18 Birch Lane",
            "November 2, 2026",
            "",
            "Dear neighbor,",
            "",
            "Thank you for lending the ladder last Sunday.",
            "Mara Ellison will return it on Friday morning.",
            "",
            "Please leave the gate unlatched.",
            "",
            "Warmly,",
            "Mara Ellison",
        ]],
    )
    write_text_pdf(
        OUT / "guide-dishwasher-care.pdf",
        [
            ["DISHWASHER CARE", "Page 1 of 3", "", "Load plates toward the center.", "Leave space around the spray arms.", "Do not block the detergent cup.", "", "A short wash is enough for glasses."],
            ["DISHWASHER CARE", "Page 2 of 3", "", "The dishwasher warranty is printed on the card", "that came in the box.", "", "Call Pell and Sons if the pump leaks.", "Have the warranty card nearby."],
            ["DISHWASHER CARE", "Page 3 of 3", "", "Twist the filter out once a month.", "Rinse it under the tap.", "Seat it until it clicks.", "", "Then run a hot rinse."],
        ],
    )
    Image.open(OUT / "receipt-northline-market.png").convert("RGB").save(OUT / "scan-market-receipt.pdf", "PDF", resolution=120.0)
    print(f"wrote {len(list(OUT.iterdir()))} files to {OUT}")


if __name__ == "__main__":
    main()
