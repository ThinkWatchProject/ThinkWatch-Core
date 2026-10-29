//! 个人信息规则的测试。**样本里的校验位在测试里照标准另算一遍**，不借被测的那份实现 ——
//! 借了的话，实现抄错了这里也跟着错。

use super::super::rules::{Rule, RuleSet, masked, scan, scan_plain};

/// 只开这一条，扫一段正文（编成 JSON 字符串扫，和网关看到的一样），返回命中的原文
fn got(id: &str, text: &str) -> Vec<String> {
    scan_plain(text, &RuleSet::only(&[id]))
        .into_iter()
        .map(|h| {
            assert_eq!(h.rule.id(), id);
            text[h.bytes].to_string()
        })
        .collect()
}

/// 只开这一条，扫一段原样的请求体
fn got_raw(id: &str, body: &str) -> Vec<String> {
    scan(body, &RuleSet::only(&[id]))
        .into_iter()
        .map(|h| body[h.bytes].to_string())
        .collect()
}

fn hits(id: &str, samples: &[(&str, &str)]) {
    for (text, want) in samples {
        assert_eq!(got(id, text), [*want], "{id} 该认出：{text}");
    }
}

fn misses(id: &str, samples: &[&str]) {
    for text in samples {
        assert!(
            got(id, text).is_empty(),
            "{id} 不该认：{text} → {:?}",
            got(id, text)
        );
    }
}

// ------------------------------------------------------------ 另算一遍的校验位

/// ISO 7064 MOD 11-2（权重 2^(17-i) mod 11）
fn id_done(first17: &str) -> String {
    let sum: u32 = first17
        .bytes()
        .enumerate()
        .map(|(i, c)| u32::from(c - b'0') * (2u32.pow(17 - i as u32) % 11))
        .sum();
    let c = (12 - sum % 11) % 11;
    let check = if c == 10 {
        'X'
    } else {
        char::from(b'0' + c as u8)
    };
    format!("{first17}{check}")
}

fn luhn_done(body: &str) -> String {
    let sum: u32 = body
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let x = u32::from(c - b'0');
            if i % 2 == 0 {
                (x * 2) / 10 + (x * 2) % 10
            } else {
                x
            }
        })
        .sum();
    format!("{body}{}", (10 - sum % 10) % 10)
}

/// IBAN：校验位先写 00，挪到末尾、字母换数，98 减去除以 97 的余数
fn iban_done(cc: &str, bban: &str) -> String {
    let moved = format!("{bban}{cc}00");
    let mut r: u64 = 0;
    for c in moved.chars() {
        let v = c.to_digit(36).unwrap() as u64;
        r = if v < 10 {
            (r * 10 + v) % 97
        } else {
            (r * 100 + v) % 97
        };
    }
    format!("{cc}{:02}{bban}", 98 - r)
}

/// ICAO 9303：7、3、1 轮着乘
fn mrz_digit(s: &str) -> char {
    let sum: u32 = s
        .chars()
        .zip([7, 3, 1].iter().cycle())
        .map(|(c, w)| {
            w * match c {
                '<' => 0,
                c => c.to_digit(36).unwrap(),
            }
        })
        .sum();
    char::from_digit(sum % 10, 10).unwrap()
}

/// 机读区第二行：各个字段拼上各自的校验位和综合校验位
fn mrz_line2(doc: &str, nat: &str, dob: &str, sex: char, exp: &str, personal: &str) -> String {
    let a = format!("{doc}{}", mrz_digit(doc));
    let b = format!("{dob}{}", mrz_digit(dob));
    let c = format!("{exp}{}", mrz_digit(exp));
    let d = format!("{personal}{}", mrz_digit(personal));
    let composite = mrz_digit(&format!("{a}{b}{c}{d}"));
    format!("{a}{nat}{b}{sex}{c}{d}{composite}")
}

fn hk_done(letters: &str, digits: &str) -> String {
    let v = |c: char| c as u32 - 'A' as u32 + 10;
    let l: Vec<char> = letters.chars().collect();
    let mut sum = if l.len() == 1 {
        36 * 9 + v(l[0]) * 8
    } else {
        v(l[0]) * 9 + v(l[1]) * 8
    };
    for (i, d) in digits.chars().enumerate() {
        sum += d.to_digit(10).unwrap() * (7 - i as u32);
    }
    let r = (11 - sum % 11) % 11;
    let check = if r == 10 {
        'A'
    } else {
        char::from_digit(r, 10).unwrap()
    };
    format!("{letters}{digits}{check}")
}

fn tw_done(first9: &str) -> String {
    let codes = "ABCDEFGHJKLMNPQRSTUVXYWZIO";
    let first = first9.chars().next().unwrap();
    let n = codes.find(first).unwrap() as u32 + 10;
    let mut sum = n / 10 + (n % 10) * 9;
    for (i, d) in first9[1..].chars().enumerate() {
        sum += d.to_digit(10).unwrap() * (8 - i as u32);
    }
    format!("{first9}{}", (10 - sum % 10) % 10)
}

fn sg_done(head: &str) -> String {
    let p = head.chars().next().unwrap();
    let mut sum: u32 = head[1..]
        .chars()
        .zip([2, 7, 6, 5, 4, 3, 2])
        .map(|(d, w)| d.to_digit(10).unwrap() * w)
        .sum();
    if p == 'T' || p == 'G' {
        sum += 4;
    }
    let table = if p == 'S' || p == 'T' {
        "JZIHGFEDCBA"
    } else {
        "XWUTRQPNMLK"
    };
    format!("{head}{}", table.as_bytes()[(sum % 11) as usize] as char)
}

fn jp_done(first11: &str) -> String {
    let d: Vec<u32> = first11.chars().map(|c| c.to_digit(10).unwrap()).collect();
    let sum: u32 = (1..=11)
        .map(|n: u32| d[11 - n as usize] * if n <= 6 { n + 1 } else { n - 5 })
        .sum();
    let r = sum % 11;
    format!("{first11}{}", if r <= 1 { 0 } else { 11 - r })
}

/// Verhoeff 生成：拿逆元表补一位
fn verhoeff_done(body: &str) -> String {
    const D: [[u8; 10]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
        [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
        [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
        [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
        [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
        [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
        [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
        [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
        [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    ];
    const P: [[u8; 10]; 8] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
        [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
        [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
        [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
        [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
        [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
        [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
    ];
    const INV: [u8; 10] = [0, 4, 3, 2, 1, 5, 6, 7, 8, 9];
    let mut c = 0u8;
    for (i, ch) in body.bytes().rev().enumerate() {
        c = D[c as usize][P[(i + 1) % 8][(ch - b'0') as usize] as usize];
    }
    format!("{body}{}", INV[c as usize])
}

fn cpf_done(nine: &str) -> String {
    let mut d: Vec<u32> = nine.chars().map(|c| c.to_digit(10).unwrap()).collect();
    for n in [9usize, 10] {
        let sum: u32 = (0..n).map(|i| d[i] * (n as u32 + 1 - i as u32)).sum();
        let r = sum % 11;
        d.push(if r < 2 { 0 } else { 11 - r });
    }
    let s: String = d
        .iter()
        .map(|x| char::from_digit(*x, 10).unwrap())
        .collect();
    format!("{}.{}.{}-{}", &s[..3], &s[3..6], &s[6..9], &s[9..])
}

fn vin_done(with_hole: &str) -> String {
    let val = |c: char| -> u32 {
        match c {
            '0'..='9' => c.to_digit(10).unwrap(),
            'A' | 'J' => 1,
            'B' | 'K' | 'S' => 2,
            'C' | 'L' | 'T' => 3,
            'D' | 'M' | 'U' => 4,
            'E' | 'N' | 'V' => 5,
            'F' | 'W' => 6,
            'G' | 'P' | 'X' => 7,
            'H' | 'Y' => 8,
            'R' | 'Z' => 9,
            _ => panic!("{c}"),
        }
    };
    let w = [8, 7, 6, 5, 4, 3, 2, 10, 0, 9, 8, 7, 6, 5, 4, 3, 2];
    let sum: u32 = with_hole
        .chars()
        .enumerate()
        .map(|(i, c)| if i == 8 { 0 } else { val(c) * w[i] })
        .sum();
    let check = match sum % 11 {
        10 => 'X',
        n => char::from_digit(n, 10).unwrap(),
    };
    let mut s: Vec<char> = with_hole.chars().collect();
    s[8] = check;
    s.into_iter().collect()
}

#[test]
fn the_test_helpers_agree_with_published_numbers() {
    // 新版外国人永久居留身份证的官方样例
    assert_eq!(id_done("93268219850101001"), "932682198501010017");
    assert_eq!(
        iban_done("DE", "370400440532013000"),
        "DE89370400440532013000"
    );
    assert_eq!(
        iban_done("GB", "WEST12345698765432"),
        "GB82WEST12345698765432"
    );
    // ICAO 9303 第 4 部分的样张
    assert_eq!(
        mrz_line2(
            "L898902C3",
            "UTO",
            "740812",
            'F',
            "120415",
            "ZE184226B<<<<<"
        ),
        "L898902C36UTO7408122F1204159ZE184226B<<<<<10"
    );
    assert_eq!(tw_done("A12345678"), "A123456789");
    assert_eq!(sg_done("S1234567"), "S1234567D");
    assert_eq!(cpf_done("529982247"), "529.982.247-25");
    assert_eq!(vin_done("1HGCM82603A004352"), "1HGCM82633A004352");
    assert_eq!(luhn_done("49015420323751"), "490154203237518");
    assert_eq!(verhoeff_done("23456789012"), "234567890124");
}

// ------------------------------------------------------------ 证件号码

#[test]
fn resident_id_covers_taiwan_permits_and_new_foreign_permanent_resident_cards() {
    let tw = id_done("83000019900307123");
    let us = id_done("91184019900307123");
    let id = "cn-resident-id";
    hits(
        id,
        &[
            ("永居证 932682198501010017", "932682198501010017"),
            (&format!("居住证号 {tw}"), &tw),
            (&format!("证件\n{us}"), &us),
        ],
    );
    misses(
        id,
        &[
            "932682198501010018",
            // 第 2–3 位要是大陆的省级代码
            &id_done("97184019900307123"),
            // 第 4–6 位要是 ISO 3166-1 的国籍代码
            &id_done("91199919900307123"),
            &id_done("84000019900307123"),
        ],
    );
}

#[test]
fn old_fifteen_digit_ids_need_a_real_prefecture_and_date() {
    let id = "cn-resident-id-15";
    hits(
        id,
        &[
            ("旧号码 110105491231002", "110105491231002"),
            ("440305850101123", "440305850101123"),
            ("身份证\n320102700615001。", "320102700615001"),
        ],
    );
    misses(
        id,
        &[
            "990105491231002",
            "110100491231002",
            "110105491331002",
            "1101054912310021",
        ],
    );
    // JSON 的数值不是
    assert!(got_raw(id, r#"{"n":110105491231002}"#).is_empty());
    assert_eq!(
        got_raw(id, r#"{"n":"110105491231002"}"#),
        ["110105491231002"]
    );
}

fn specimen_mrz() -> (String, String) {
    let line1 = format!("{:<<44}", "P<UTOERIKSSON<<ANNA<MARIA");
    let line2 = mrz_line2(
        "L898902C3",
        "UTO",
        "740812",
        'F',
        "120415",
        "ZE184226B<<<<<",
    );
    (line1, line2)
}

#[test]
fn a_passport_mrz_is_taken_as_both_lines() {
    let (l1, l2) = specimen_mrz();
    let other1 = format!("{:<<44}", "PDCHNZHANG<<SAN");
    let other2 = mrz_line2(
        "E12345678",
        "CHN",
        "900307",
        'M',
        "300101",
        "<<<<<<<<<<<<<<",
    );
    let id = "passport-mrz";
    let whole = format!("{l1}\n{l2}");
    let crlf = format!("{l1}\r\n{l2}");
    let zhang = format!("{other1}\n{other2}");
    hits(
        id,
        &[
            (&format!("护照页：\n{whole}\n谢谢"), &whole),
            (&crlf, &crlf),
            (&format!("MRZ:{zhang}"), &zhang),
        ],
    );
    let mut bad = l2.clone();
    bad.replace_range(9..10, "7");
    misses(
        id,
        &[
            &format!("{l1}\n{bad}"),
            &l1,
            &format!("{l1}\n{}", &l2[..43]),
            &format!("{}\n{l2}", l1.replacen('P', "I", 1)),
            &format!("{l1} {l2}"),
        ],
    );
    // 请求体原样：换行是 `\n` 两个字符
    let body = serde_json::json!({ "content": format!("护照页\n{whole}") }).to_string();
    assert_eq!(got_raw(id, &body), [format!("{l1}\\n{l2}")]);
    assert_eq!(masked(&Rule::Builtin("passport-mrz"), &whole), "…02C3");
}

#[test]
fn a_chinese_passport_number_needs_the_word_passport_nearby() {
    let id = "cn-passport";
    hits(
        id,
        &[
            ("护照号码：E12345678", "E12345678"),
            ("Passport No. EA1234567", "EA1234567"),
            ("护照\nE98765432", "E98765432"),
        ],
    );
    misses(
        id,
        &[
            "E12345678",
            "护照：E1234567",
            "护照：EI1234567",
            "护照号我忘了，订单号写在这张纸的背面左下角那一栏：E12345678",
        ],
    );
    // Python 客户端把中文写成 `\uXXXX`：关键词照样认得出
    let body = r#"{"c":"护照：E12345678"}"#;
    assert_eq!(got_raw(id, body), ["E12345678"]);
}

#[test]
fn hong_kong_macao_and_taiwan_ids_check_out() {
    let hk1 = hk_done("A", "123456");
    let hk2 = hk_done("XY", "987654");
    let (hk_body, hk_check) = hk1.split_at(7);
    let hk_paren = format!("{hk_body}({hk_check})");
    let tw = tw_done("F22345678");
    let resident = tw_done("A80000001");
    let id = "hk-macau-taiwan-id";
    hits(
        id,
        &[
            (&format!("香港身份证 {hk_paren}"), &hk_paren),
            (&hk2, &hk2),
            ("A123456789", "A123456789"),
            (&tw, &tw),
            (&resident, &resident),
            ("澳门身份证：1234567(8)", "1234567(8)"),
        ],
    );
    let mut wrong_hk = hk2.clone();
    wrong_hk.pop();
    wrong_hk.push(if hk2.ends_with('1') { '2' } else { '1' });
    misses(
        id,
        &[
            &wrong_hk,
            "A123456788",
            "A323456789",
            "1234567(8)",
            "澳门身份证：2234567(8)",
        ],
    );
}

#[test]
fn a_travel_permit_needs_its_name_nearby() {
    let id = "mainland-travel-permit";
    hits(
        id,
        &[
            ("回乡证号码 H12345678", "H12345678"),
            ("港澳通行证：M1234567801", "M1234567801"),
            ("台胞证 12345678", "12345678"),
        ],
    );
    misses(
        id,
        &[
            "H12345678",
            "回乡证 H1234567",
            "台胞证 1234567",
            "通行证 12345678",
        ],
    );
}

// ------------------------------------------------------------ 境外证件

#[test]
fn us_social_security_numbers() {
    hits(
        "us-ssn",
        &[
            ("SSN 536-22-1234", "536-22-1234"),
            ("001-01-0001", "001-01-0001"),
            ("ssn:\n772-81-4499", "772-81-4499"),
        ],
    );
    misses(
        "us-ssn",
        &[
            "000-12-3456",
            "666-12-3456",
            "912-34-5678",
            "123-00-4567",
            "123-45-0000",
            "078-05-1120",
            "536221234",
            "2023-10-29",
        ],
    );
}

#[test]
fn uk_national_insurance_numbers() {
    hits(
        "uk-nino",
        &[
            ("NINO AB123456C", "AB123456C"),
            ("AB 12 34 56 C", "AB 12 34 56 C"),
            ("PX654321D.", "PX654321D"),
        ],
    );
    misses(
        "uk-nino",
        &[
            "DA123456A",
            "AO123456A",
            "GB123456A",
            "AB123456E",
            "AB12345C",
            "ab123456c",
        ],
    );
}

#[test]
fn singapore_nric_and_fin() {
    let t = sg_done("T0123456");
    let f = sg_done("F7654321");
    let g = sg_done("G1111112");
    hits(
        "sg-nric",
        &[("S1234567D", "S1234567D"), (&t, &t), (&f, &f), (&g, &g)],
    );
    let mut wrong = t.clone();
    wrong.pop();
    wrong.push(if t.ends_with('A') { 'B' } else { 'A' });
    misses("sg-nric", &[&wrong, "M1234567K", "S123456D", "S1234567"]);
}

#[test]
fn korean_resident_registration_numbers_check_the_date_and_gender_only() {
    hits(
        "kr-rrn",
        &[
            ("900101-1234567", "900101-1234567"),
            ("주민번호 010203-4123456", "010203-4123456"),
            ("851231-2345678", "851231-2345678"),
            ("900101-5000000", "900101-5000000"),
        ],
    );
    misses(
        "kr-rrn",
        &[
            "901301-1234567",
            "900230-1234567",
            "900101-9234567",
            "900101-0234567",
            "991231-3123456",
            "900101-123456",
            "9001011234567",
        ],
    );
}

#[test]
fn japanese_my_number_needs_the_word_nearby() {
    let n = jp_done("12345678901");
    let spaced = format!("{} {} {}", &n[..4], &n[4..8], &n[8..]);
    let dashed = format!("{}-{}-{}", &n[..4], &n[4..8], &n[8..]);
    let id = "jp-my-number";
    hits(
        id,
        &[
            (&format!("マイナンバー：{n}"), &n),
            (&format!("個人番号 {spaced}"), &spaced),
            (&format!("My Number: {dashed}"), &dashed),
        ],
    );
    let mut wrong = n.clone();
    wrong.pop();
    wrong.push(if n.ends_with('1') { '2' } else { '1' });
    misses(
        id,
        &[
            &n,
            &format!("マイナンバー：{wrong}"),
            &format!("マイナンバーは別の書類に書いてありますので確認してください {n}"),
        ],
    );
}

#[test]
fn aadhaar_numbers_pass_verhoeff() {
    let a = verhoeff_done("23456789012");
    let b = verhoeff_done("98765432101");
    let spaced = format!("{} {} {}", &b[..4], &b[4..8], &b[8..]);
    let dashed = format!("{}-{}-{}", &a[..4], &a[4..8], &a[8..]);
    hits(
        "in-aadhaar",
        &[
            (&a, &a),
            (&format!("Aadhaar {spaced}"), &spaced),
            (&dashed, &dashed),
        ],
    );
    let one = verhoeff_done("13456789012");
    let mut wrong = a.clone();
    wrong.pop();
    wrong.push(if a.ends_with('1') { '2' } else { '1' });
    misses(
        "in-aadhaar",
        &[
            &one,
            &wrong,
            &format!("{spaced} 1234"),
            &format!("4321 {spaced}"),
            "222222222222",
        ],
    );
}

#[test]
fn brazilian_cpf_must_be_written_with_its_dots() {
    let a = cpf_done("123456789");
    let b = cpf_done("987654321");
    hits(
        "br-cpf",
        &[("CPF 529.982.247-25", "529.982.247-25"), (&a, &a), (&b, &b)],
    );
    misses(
        "br-cpf",
        &[
            "111.111.111-11",
            "529.982.247-26",
            "52998224725",
            "529.982.24725",
        ],
    );
}

// ------------------------------------------------------------ 金融账户

#[test]
fn ibans_check_country_length_structure_and_mod_97() {
    let de = iban_done("DE", "500105175407324931");
    let gb = iban_done("GB", "NWBK60161331926820");
    let fr = iban_done("FR", "20041010050500013M02607");
    let de_grouped: String = de
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(" ");
    hits(
        "iban",
        &[
            (&format!("IBAN: {de_grouped}"), &de_grouped),
            (&format!("账户\n{gb}"), &gb),
            (&fr, &fr),
        ],
    );
    let mut wrong = de.clone();
    wrong.pop();
    wrong.push(if de.ends_with('1') { '2' } else { '1' });
    misses(
        "iban",
        &[
            &wrong,
            &de[..de.len() - 1],
            &format!("XX{}", &de[2..]),
            &de.to_lowercase(),
            // 德国的 BBAN 只有数字
            &iban_done("DE", "50010517540732493A"),
            // 文档里人人都贴的样例
            "DE89 3704 0044 0532 0130 00",
            "GB82WEST12345698765432",
        ],
    );
    assert!(super::super::rules::builtin("iban").unwrap().on_by_default);
}

// ------------------------------------------------------------ 联系方式

#[test]
fn mobile_numbers_with_a_country_code_or_written_in_groups() {
    let id = "cn-mobile-phone";
    hits(
        id,
        &[
            ("电话 +86 138 1234 5678", "138 1234 5678"),
            ("+8613812345678", "13812345678"),
            ("0086-138-1234-5678", "138-1234-5678"),
            ("138 1234 5678。", "138 1234 5678"),
            ("手机：\n138-1234-5678", "138-1234-5678"),
            ("008613812345678", "13812345678"),
        ],
    );
    misses(
        id,
        &[
            "6222 138 1234 5678",
            "138 1234 5678 9012",
            "128-1234-5678",
            "8613812345678",
            "138-12345-678",
        ],
    );
}

#[test]
fn landlines_need_a_known_area_code_and_a_separator() {
    let id = "cn-landline";
    hits(
        id,
        &[
            ("电话 010-62345678", "010-62345678"),
            ("(0755)2345678", "(0755)2345678"),
            ("（021） 87654321", "（021） 87654321"),
            ("0755-82345678", "0755-82345678"),
        ],
    );
    misses(
        id,
        &[
            "010-1234567",
            "0999-1234567",
            "2023-10-29",
            "010-02345678",
            "(0755) 0234567",
            "01012345678",
        ],
    );
}

#[test]
fn international_numbers_need_a_known_country_code_and_length() {
    let id = "intl-phone";
    hits(
        id,
        &[
            ("call +1 (415) 555-2671 now", "+1 (415) 555-2671"),
            ("+44 20 7946 0958", "+44 20 7946 0958"),
            ("tel:\n+81-3-1234-5678", "+81-3-1234-5678"),
            ("+8613812345678", "+8613812345678"),
        ],
    );
    misses(
        id,
        &[
            "1e+10",
            "+1 555 1234",
            "+999 12345678",
            "+08:00",
            "x+12025550123",
            "+1 415 555 26710",
        ],
    );
}

// ------------------------------------------------------------ 住址与位置

#[test]
fn a_street_address_needs_a_division_a_road_and_a_door_number() {
    let id = "cn-address";
    hits(
        id,
        &[
            ("寄到北京市朝阳区建国路88号，谢谢", "北京市朝阳区建国路88号"),
            (
                "广东省深圳市南山区科苑路15号3栋201室",
                "广东省深圳市南山区科苑路15号3栋201室",
            ),
            ("地址：\n杭州市西湖区文三路90号", "杭州市西湖区文三路90号"),
            ("上海浦东新区碧波路690号", "上海浦东新区碧波路690号"),
        ],
    );
    misses(
        id,
        &[
            "北京市朝阳区",
            "朝阳区有很多路",
            "深圳市地铁1号线",
            "北京市朝阳区，建国路1号",
            "城市道路1号",
        ],
    );
    let r = Rule::Builtin("cn-address");
    assert_eq!(
        masked(&r, "广东省深圳市南山区科苑路15号"),
        "广东省深圳市南山区…"
    );
    assert_eq!(masked(&r, "某地科苑路15号"), "…");
}

#[test]
fn coordinates_come_in_pairs_of_precise_numbers() {
    let id = "geo-coordinates";
    for (text, want) in [
        ("位置 31.23045, 121.47370", ["31.23045", "121.47370"]),
        (
            "lat: 31.230416, lng: 121.473701",
            ["31.230416", "121.473701"],
        ),
        ("[-122.41942, 37.77493]", ["-122.41942", "37.77493"]),
    ] {
        assert_eq!(got(id, text), want, "{text}");
    }
    misses(
        id,
        &[
            "0.12345, 0.67890",
            "[0.123456, 0.234567, 0.345678]",
            "31.23, 121.47",
            "91.12345, 200.12345",
            "31.23045, 121.47370, 5.12345",
            "31.23045",
        ],
    );
    // 请求体本身的数值不换（换了就不是 JSON 了），字符串里的换
    assert!(got_raw(id, r#"{"lat":31.23045,"lng":121.47370}"#).is_empty());
    assert_eq!(
        got_raw(id, r#"{"lat":"31.23045","lng":"121.47370"}"#),
        ["31.23045", "121.47370"]
    );
    assert_eq!(masked(&Rule::Builtin(id), "121.473701"), "121.47…");
}

// ------------------------------------------------------------ 车辆

#[test]
fn licence_plates_start_with_a_province() {
    let id = "cn-license-plate";
    hits(
        id,
        &[
            ("车牌京A12345", "京A12345"),
            ("粤B·D12345", "粤B·D12345"),
            ("沪A 12345D", "沪A 12345D"),
            ("苏E1A23B", "苏E1A23B"),
        ],
    );
    misses(
        id,
        &["京A1234", "京AI2345", "京ABCDE1", "A12345", "京A123456"],
    );
}

#[test]
fn a_vin_passes_its_ninth_digit() {
    let a = vin_done("LSVAU2180N2183294");
    let b = vin_done("WVWZZZ1JZ3W386752");
    hits(
        "vin",
        &[
            ("VIN 1HGCM82633A004352", "1HGCM82633A004352"),
            (&a, &a),
            (&b, &b),
        ],
    );
    let mut wrong = a.clone().into_bytes();
    wrong[8] = if wrong[8] == b'1' { b'2' } else { b'1' };
    misses(
        "vin",
        &[
            std::str::from_utf8(&wrong).unwrap(),
            "1HGCM82633A00435",
            "1HGCM82633I004352",
            &vin_done("1234567890ABCDEF1"),
        ],
    );
}

// ------------------------------------------------------------ 设备

#[test]
fn an_imei_needs_the_word_imei_nearby() {
    let other = luhn_done("35209900176148");
    hits(
        "imei",
        &[
            ("IMEI: 490154203237518", "490154203237518"),
            (&format!("imei号 {other}"), &other),
            ("设备 IMEI\n490154203237518", "490154203237518"),
        ],
    );
    misses(
        "imei",
        &[
            "490154203237518",
            "IMEI: 490154203237519",
            "IMEI 在包装盒背面的标签上可以找到，序列号 490154203237518",
        ],
    );
}

#[test]
fn mac_addresses_of_real_devices() {
    hits(
        "mac-address",
        &[
            ("MAC 00:1A:2B:3C:4D:5E", "00:1A:2B:3C:4D:5E"),
            ("00-1a-2b-3c-4d-5e", "00-1a-2b-3c-4d-5e"),
            ("网卡\n3c:22:fb:12:34:56。", "3c:22:fb:12:34:56"),
        ],
    );
    misses(
        "mac-address",
        &[
            "00:00:00:00:00:00",
            "ff:ff:ff:ff:ff:ff",
            "01:00:5e:00:00:fb",
            "12:34:56",
            "00:1A:2B:3C:4D:5E:6F",
            "fe80::1a:2b:3c:4d:5e",
        ],
    );
}

#[test]
fn public_addresses_but_not_private_documentation_or_versions() {
    hits(
        "public-ip",
        &[
            ("出口 52.12.34.56", "52.12.34.56"),
            ("2404:6800:4005:80a::200e", "2404:6800:4005:80a::200e"),
            ("ip:\n114.80.1.2.", "114.80.1.2"),
        ],
    );
    misses(
        "public-ip",
        &[
            "192.168.1.1",
            "100.64.0.1",
            "203.0.113.5",
            "2001:db8::1",
            "version 10.2.3.4",
            "OIDC Core 3.1.3.7",
            "WSL 2.3.26.0",
            "8.8.8.8",
            "fe80::1",
            "fd12:3456::1",
            "010.1.2.3",
            "std::fmt::Display",
        ],
    );
}

#[test]
fn internal_addresses_now_include_cgnat_ula_and_more_suffixes() {
    hits(
        "internal-ip",
        &[
            ("tailscale 100.64.1.2", "100.64.1.2"),
            ("fd12:3456:789a::1", "fd12:3456:789a::1"),
            ("100.127.255.1", "100.127.255.1"),
        ],
    );
    misses(
        "internal-ip",
        &["100.128.0.1", "100.63.0.1", "fe80::1", "2404:6800::1"],
    );
    hits(
        "internal-domain",
        &[
            ("jenkins.corp", "jenkins.corp"),
            ("nas.home.arpa", "nas.home.arpa"),
            ("wiki.intranet", "wiki.intranet"),
        ],
    );
    misses("internal-domain", &["corp", "my.corporation", "x.arpa"]);
}

// ------------------------------------------------------------ 按字段名认的

#[test]
fn names_by_field_name() {
    let id = "labeled-name";
    hits(
        id,
        &[
            (r#"{"real_name": "张三"}"#, "张三"),
            ("姓名：李四\n电话：13800138000", "李四"),
            (r#"full_name = "Ada Lovelace""#, "Ada Lovelace"),
            ("患者姓名: 王五", "王五"),
            ("realName: Grace Hopper", "Grace Hopper"),
        ],
    );
    misses(
        id,
        &[
            r#"{"name": "get_weather", "description": "天气"}"#,
            "real_name: String,",
            "full_name = user.full_name",
            r#"{"real_name": "${NAME}"}"#,
            "姓名：",
            r#"{"real_name": "string"}"#,
        ],
    );
    // 请求体本身：裸的 name 有旁证（同一个对象里的 phone）才算
    let body = r#"{"name":"王五","phone":"13800138000","tools":[{"name":"get_weather"}]}"#;
    assert_eq!(got_raw(id, body), ["王五"]);
    let body =
        r#"{"model":"m","name":"assistant","tools":[{"name":"x","input":{"email":"a@b.cn"}}]}"#;
    assert!(got_raw(id, body).is_empty(), "{:?}", got_raw(id, body));
    // 正文里的一段 JSON
    assert_eq!(
        got(id, r#"{"name": "赵六", "email": "z@example.com"}"#),
        ["赵六"]
    );
    // YAML：前后两行以内有旁证
    assert_eq!(got(id, "name: 孙七\nmobile: 13800138000"), ["孙七"]);
    assert!(got(id, "name: my-service\nimage: nginx").is_empty());
    // 不是 JSON 的一段话从字段名开始
    assert_eq!(got_raw(id, "name: 孙七\nmobile: 13800138000"), ["孙七"]);
    // 内联的映射里的键不在行首，不作证
    assert!(
        got(
            id,
            "actions: { email: record }\ncustom:\n  - name: 内部项目号"
        )
        .is_empty()
    );
    assert_eq!(masked(&Rule::Builtin(id), "张三"), "张…");
}

#[test]
fn addresses_by_field_name() {
    let id = "labeled-address";
    hits(
        id,
        &[
            (
                "收货地址：上海市徐汇区漕溪北路88号",
                "上海市徐汇区漕溪北路88号",
            ),
            (
                r#"{"home_address": "1 Main St, Springfield"}"#,
                "1 Main St, Springfield",
            ),
            (
                "phone: 13800138000\naddress: 北京市海淀区中关村大街1号",
                "北京市海淀区中关村大街1号",
            ),
        ],
    );
    misses(
        id,
        &[
            r#"{"address": "10.0.0.1", "name": "db"}"#,
            "address = \"0.0.0.0:8080\"",
            "listen_address: 127.0.0.1",
            r#"{"shipping_address": "<address>"}"#,
        ],
    );
    let r = Rule::Builtin(id);
    assert_eq!(masked(&r, "上海市徐汇区漕溪北路88号"), "上海市徐汇区…");
    assert_eq!(masked(&r, "1 Main St, Springfield"), "…");
}

#[test]
fn birth_dates_accounts_and_health_by_field_name() {
    hits(
        "labeled-birth-date",
        &[
            (r#"{"dob": "1990-01-01"}"#, "1990-01-01"),
            ("出生日期：1990年1月1日", "1990年1月1日"),
            ("date_of_birth: 1985-12-31", "1985-12-31"),
        ],
    );
    misses(
        "labeled-birth-date",
        &[
            "dob: date",
            r#"{"birthday": "string"}"#,
            "birthday: ${DOB}",
            "birthday: Optional[date]",
        ],
    );
    hits(
        "labeled-account",
        &[
            (
                r#"{"account_number": "6217000012345678"}"#,
                "6217000012345678",
            ),
            (
                "银行账号：6217 0000 1234 5678 901",
                "6217 0000 1234 5678 901",
            ),
            ("bank_account=1234567890&x=1", "1234567890"),
        ],
    );
    misses(
        "labeled-account",
        &[
            "account_number: u64",
            r#"{"account_number": "xxxx"}"#,
            "bank_account: 12345",
        ],
    );
    hits(
        "labeled-health",
        &[
            ("诊断：2 型糖尿病", "2 型糖尿病"),
            (
                r#"{"diagnosis": "hypertension stage 2"}"#,
                "hypertension stage 2",
            ),
            ("既往病史: 高血压十年", "高血压十年"),
        ],
    );
    misses(
        "labeled-health",
        &["diagnosis: str", "诊断：无", "diagnosis = report.diagnosis"],
    );
    assert!(got_raw("labeled-health", r#"{"diagnosis":null}"#).is_empty());
    assert_eq!(masked(&Rule::Builtin("labeled-health"), "高血压"), "…");
    assert_eq!(
        masked(&Rule::Builtin("labeled-birth-date"), "1990-01-01"),
        "…"
    );
    assert_eq!(
        masked(&Rule::Builtin("labeled-account"), "6217000012345678"),
        "…5678"
    );
}

#[test]
fn every_new_personal_rule_ships_with_the_agreed_label_and_default() {
    use super::super::rules::{Kind, builtin};
    for (id, kind, label, on) in [
        (
            "cn-resident-id-15",
            Kind::IdDocuments,
            "TW_ID_NUMBER",
            false,
        ),
        ("passport-mrz", Kind::IdDocuments, "TW_PASSPORT", true),
        ("cn-passport", Kind::IdDocuments, "TW_PASSPORT", false),
        (
            "hk-macau-taiwan-id",
            Kind::IdDocuments,
            "TW_ID_NUMBER",
            false,
        ),
        (
            "mainland-travel-permit",
            Kind::IdDocuments,
            "TW_ID_NUMBER",
            false,
        ),
        ("us-ssn", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("uk-nino", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("sg-nric", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("kr-rrn", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("jp-my-number", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("in-aadhaar", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("br-cpf", Kind::ForeignIds, "TW_ID_NUMBER", false),
        ("iban", Kind::Financial, "TW_ACCOUNT_NUMBER", true),
        ("cn-landline", Kind::Contact, "TW_PHONE", false),
        ("intl-phone", Kind::Contact, "TW_PHONE", false),
        ("cn-address", Kind::Location, "TW_ADDRESS", false),
        ("geo-coordinates", Kind::Location, "TW_LOCATION", false),
        ("cn-license-plate", Kind::Vehicle, "TW_PLATE", false),
        ("vin", Kind::Vehicle, "TW_VIN", false),
        ("imei", Kind::Device, "TW_DEVICE_ID", false),
        ("mac-address", Kind::Device, "TW_DEVICE_ID", false),
        ("public-ip", Kind::Device, "TW_IP", false),
        ("labeled-name", Kind::LabeledPersonal, "TW_NAME", false),
        (
            "labeled-address",
            Kind::LabeledPersonal,
            "TW_ADDRESS",
            false,
        ),
        (
            "labeled-birth-date",
            Kind::LabeledPersonal,
            "TW_BIRTH_DATE",
            false,
        ),
        (
            "labeled-account",
            Kind::LabeledPersonal,
            "TW_ACCOUNT_NUMBER",
            false,
        ),
        ("labeled-health", Kind::LabeledPersonal, "TW_HEALTH", false),
    ] {
        let b = builtin(id).unwrap_or_else(|| panic!("没有 {id}"));
        assert_eq!(
            (b.kind, b.label, b.on_by_default),
            (kind, Some(label), on),
            "{id}"
        );
        assert!(b.kind.is_personal(), "{id}");
    }
}

#[test]
fn personal_placeholders_say_what_they_were() {
    let set = RuleSet::only(&["iban", "passport-mrz", "labeled-name"]);
    let (l1, l2) = specimen_mrz();
    let iban = iban_done("DE", "500105175407324931");
    let body = serde_json::json!({
        "content": format!("{{\"real_name\": \"张三\"}} {iban}\n{l1}\n{l2}")
    })
    .to_string();
    let r = crate::redact::replace::redact(
        &body,
        &set,
        crate::redact::replace::Ledger::new(crate::redact::replace::Scheme::SECRET),
    );
    let v: serde_json::Value = serde_json::from_str(&r.text).unwrap();
    assert_eq!(
        v["content"],
        "{\"real_name\": \"<<TW_NAME_1>>\"} <<TW_ACCOUNT_NUMBER_1>>\n<<TW_PASSPORT_1>>"
    );
    assert_eq!(crate::redact::replace::restore(&r.text, &r.ledger), body);
}

/// 每条代码判据的个人信息规则的一个命中样本，给 `every_builtin_rule_describes_what_it_matches`
pub(in crate::redact) fn sample(id: &str) -> Option<String> {
    let (l1, l2) = specimen_mrz();
    Some(match id {
        "cn-resident-id-15" => "110105491231002".into(),
        "passport-mrz" => format!("{l1}\n{l2}"),
        "cn-passport" => "护照 E12345678".into(),
        "hk-macau-taiwan-id" => "A123456789".into(),
        "mainland-travel-permit" => "回乡证 H12345678".into(),
        "us-ssn" => "536-22-1234".into(),
        "uk-nino" => "AB123456C".into(),
        "sg-nric" => "S1234567D".into(),
        "kr-rrn" => "900101-1234567".into(),
        "jp-my-number" => format!("マイナンバー {}", jp_done("12345678901")),
        "in-aadhaar" => verhoeff_done("23456789012"),
        "br-cpf" => "529.982.247-25".into(),
        "iban" => iban_done("DE", "500105175407324931"),
        "cn-landline" => "010-62345678".into(),
        "intl-phone" => "+44 20 7946 0958".into(),
        "cn-address" => "北京市朝阳区建国路88号".into(),
        "geo-coordinates" => "31.23045, 121.47370".into(),
        "cn-license-plate" => "京A12345".into(),
        "vin" => "1HGCM82633A004352".into(),
        "imei" => "IMEI 490154203237518".into(),
        "mac-address" => "00:1A:2B:3C:4D:5E".into(),
        "public-ip" => "52.12.34.56".into(),
        _ => return None,
    })
}

/// 误报基准：把几个源码目录里的每个文件当成一段贴进对话的正文扫一遍（所有个人信息规则都开），
/// 按规则数命中。`TW_FP_PATHS` 是用 `:` 隔开的几个目录。不随测试跑：
///
/// ```sh
/// TW_FP_PATHS=/path/a:/path/b cargo test -p tw-guard --release false_positive_survey -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn false_positive_survey() {
    use super::super::rules::{BUILTINS, Kind};
    use std::collections::BTreeMap;
    let Ok(paths) = std::env::var("TW_FP_PATHS") else {
        return;
    };
    let mine: Vec<&str> = BUILTINS
        .iter()
        .filter(|b| b.kind.is_personal() || b.kind == Kind::Internal)
        .map(|b| b.id)
        .collect();
    let set = RuleSet::only(&mine);
    const SKIP: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "dist",
        "build",
        "fixtures",
        "testdata",
        "__fixtures__",
        "__snapshots__",
        ".next",
        "vendor",
        "tests",
        "test",
        ".astro",
        "coverage",
    ];
    let mut files = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = paths.split(':').map(Into::into).collect();
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if ft.is_file()
                && !name.ends_with("tests.rs")
                && !name.contains(".test.")
                && !name.contains(".spec.")
                && !name.ends_with(".lock")
                && !name.ends_with("lock.yaml")
                && !name.ends_with("lock.json")
            {
                files.push(path);
            }
        }
    }
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut bytes = 0usize;
    for f in &files {
        let Ok(data) = std::fs::read(f) else { continue };
        if data.len() > 2 << 20 || data.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(data) else {
            continue;
        };
        bytes += text.len();
        for h in scan_plain(&text, &set) {
            let v = &text[h.bytes.clone()];
            let mut id = h.rule.id().to_string();
            // 身份证号那条单数扩充出来的（`9`、`83` 开头）：原来的判据不归这次管
            if id == "cn-resident-id" && (v.starts_with('9') || v.starts_with("83")) {
                id = "cn-resident-id[9/83]".to_string();
            }
            *count.entry(id.clone()).or_default() += 1;
            let ex = examples.entry(id.clone()).or_default();
            if ex.len() < 8 {
                ex.push(format!("{}: {:?}", f.display(), &text[h.bytes]));
            }
        }
    }
    println!("{} files, {} KiB", files.len(), bytes / 1024);
    for id in mine.iter().copied().chain(["cn-resident-id[9/83]"]) {
        let id = &id;
        println!("{id}: {}", count.get(*id).copied().unwrap_or(0));
        for e in examples.get(*id).into_iter().flatten() {
            println!("    {e}");
        }
    }
}
