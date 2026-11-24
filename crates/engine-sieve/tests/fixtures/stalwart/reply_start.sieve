require ["vacation", "relational", "date"];

if currentdate :value "ge" "iso8601" "2030-01-02T09:00:00Z" {
vacation :mime :subject "Away" "Content-Type: text/plain; charset=\"utf-8\"
Content-Transfer-Encoding: 7bit

I am away.";
}
