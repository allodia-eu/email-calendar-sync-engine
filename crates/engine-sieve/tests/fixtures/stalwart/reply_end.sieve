require ["vacation", "relational", "date"];

if currentdate :value "le" "iso8601" "2030-01-09T17:30:00Z" {
vacation :mime :subject "Away" "Content-Type: text/plain; charset=\"utf-8\"
Content-Transfer-Encoding: 7bit

I am away.";
}
