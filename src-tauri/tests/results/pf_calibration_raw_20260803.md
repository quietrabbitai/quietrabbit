# Privacy Filter Confidence Threshold Calibration

Run against live privacy-filter.cpp model. items.id=36 / decisions.id=405.

Privacy Filter: available and model loaded.

| id | group | expected | detected spans (label:score) | tier (current thresholds) |
|---|---|---|---|---|
| email-plain | structural | private_email | private_email:1.000 "jason.kulaga@gmail.com" | Easy |
| email-plus-addressed | structural | private_email | private_email:1.000 "j.kulaga+work@protonmail.com" | Easy |
| phone-dashed | structural | private_phone | private_phone:1.000 "555-234-8891" | Easy |
| phone-intl | structural | private_phone | private_phone:1.000 "+1 555 234 8891" | Easy |
| account-number-bank | structural | account_number | account_number:1.000 "4521-8890-1123-4567" | High (forced: severity/tier) |
| account-number-routing | structural | account_number | account_number:1.000 "021000021"; account_number:1.000 "8834471290" | High (forced: severity/tier) |
| person-full-name | structural-non-easy | private_person | private_person:1.000 "Jason Kulaga" | Medium |
| person-intro | structural-non-easy | private_person | private_person:1.000 "Sarah Chen" | Medium |
| address-full | structural-non-easy | private_address | private_address:0.850 "742 Evergreen Terrace, Springfield." | Medium |
| address-street-only | structural-non-easy | private_address | private_address:0.992 "12 Baker Street, Garuda City" | Medium |
| name-common-word-rose | ambiguous | private_person | private_person:1.000 "Rose"; private_date:0.605 "next week" | Medium |
| name-common-word-bill | ambiguous | private_person | private_person:0.998 "Bill" | Medium |
| name-embedded-family | ambiguous | private_person | private_person:1.000 "Grace" | Medium |
| name-nickname-only | ambiguous | private_person | (none) | n/a (missed) |
| financial-income | contextual | (none) | (none) | n/a (correctly none) |
| financial-debt | contextual | (none) | (none) | n/a (correctly none) |
| medical-diagnosis | contextual | (none) | private_date:1.000 "2019" | High (forced: severity/tier) (unexpected detection) |
| medical-medication | contextual | (none) | (none) | n/a (correctly none) |
| dietary-health-context | contextual | (none) | (none) | n/a (correctly none) |
| personal-history-divorce | contextual | (none) | (none) | n/a (correctly none) |
| secret-wifi-password | secret | secret | secret:0.836 "Sunflower88!" | High (forced: severity/tier) |
| secret-ssn | secret | secret | account_number:1.000 "123-45-6789" | n/a (missed) |
| url-personal-site | url-date | private_url | private_url:1.000 "https://jasonkulaga.dev" | Medium |
| url-linkedin | url-date | private_url | private_url:1.000 "linkedin.com/in/jasonkulaga" | Medium |
| date-birthday | url-date | private_date | private_date:1.000 "March 3, 1990" | Medium |
| date-anniversary | url-date | private_date | (none) | n/a (missed) |
| negative-weather | negative-control | (none) | (none) | n/a (correctly none) |
| negative-business | negative-control | (none) | (none) | n/a (correctly none) |
| negative-generic-task | negative-control | (none) | (none) | n/a (correctly none) |

## Summary

Total cases: 29
Cases with expected detection: 20
Negative-control cases: 9

**False negatives (expected span, PF found NOTHING) -- critical per decisions.id=405: 2**
  - name-nickname-only
  - date-anniversary

**Wrong-category detections (expected span found under a different label): 1**
  - secret-ssn: got ["account_number"]

False positives on negative controls (acceptable per decisions.id=405, informational only): 1
  - medical-diagnosis
